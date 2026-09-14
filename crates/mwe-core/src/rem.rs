// SPDX-License-Identifier: AGPL-3.0-or-later
//! REM (Reorganization Memory) nightly cycle.
//!
//! Cron-driven job that runs without users connected. The write-jobs
//! skip smart-family wikis (the smart consumer owns those writes via
//! `wiki_admin_push`); the one sub-job that walks *only* smart wikis is
//! the briefing dispatcher, which reads them and surfaces what it finds
//! as `_briefing.md` items for the smart consumer to triage.
//!
//! ## Sub-jobs
//!
//! **[`run_cycle`] is the SSOT for the sub-job roster and their fixed
//! order — read its body**; each sub-job's own contract is on its own
//! `run_*` function. The shape
//! of the cycle: the two proposal sweeps settle overdue
//! `structure_proposals` first; the consolidation and hygiene sweeps
//! (dedup, promote, merge, completion, contradiction, the page judge,
//! refile, provenance, dates) reorganise the fact set act-first; and the
//! archive detector and the briefing dispatcher emit proposals / briefing
//! items.
//!
//! ## Cycle invariants
//!
//! - Sub-jobs run in the fixed order wired in [`run_cycle`]; ordering
//!   is load-bearing only at the edges (the proposal sweeps settle
//!   pending state before the write-jobs touch it, and provenance
//!   hygiene runs right before the date normalizer so later sub-jobs
//!   see pointer-clean text).
//! - Every state-mutating sub-step is journaled in `rem_ops_log` via
//!   [`crate::wal::begin_rem_op`] → `complete_rem_op` / `fail_rem_op`.
//!   The floor's sub-step inverses are idempotent (`atomic_write`
//!   handles partial page writes; `mark_superseded` and
//!   `mark_forgotten` are no-ops on already-superseded / already-
//!   tombstoned rows; `insert_event` is gated by an idempotency probe).
//!   A crashed cycle is safe to retry on the next REM tick: the boot
//!   sweep ([`crate::wal::rollback_stale_rems`]) closes the rows the
//!   crash left open and the next cycle re-does the work.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures_util::StreamExt as _;
use serde_json::json;
use sqlx::SqlitePool;
use thiserror::Error;

pub mod briefing_processor;
pub mod day;

use crate::archive::{self, ArchiveError};
use crate::briefing::{self, BriefingError, BriefingSourceKind, NotifyRequest};
use crate::dedup::{self, DedupMergeHints};
use crate::embedder::Embedder;
use crate::events::EventsError;
use crate::events::{self, EventKind};
use crate::fact_index::{self, FactIndexRow};
use crate::llm::{CompletionRequest, LlmBackend};
use crate::planner::{CompilationPlan, PagePlan};
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
    /// Maximum number of dedup supersedes per cycle — REM never
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
    /// sub-job applies per cycle. 5/night by default — REM never
    /// carpet-bombs the operator's inbox.
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
    /// The same floor for a **`prosa-tecnica`** page — short bullets with
    /// brief descriptions, scanned by points rather than read as a thread.
    ///
    /// Scanning tolerates more mass than following a narrative, so the floor
    /// is four times the narrative one (founder, 2026-08-25: a technical page
    /// is a reference, and a reference that keeps halving itself stops being
    /// one). A **`lista`** page has no floor at
    /// all: it is *consulted*, not read, and its value is being complete in
    /// one place — splitting it by size breaks the only thing it is for, and
    /// leaves two halves, neither of which answers the question. See
    /// [`mass_floor_for_style`].
    pub auto_promote_min_page_facts_technical: usize,
    /// Minimum **group size**, in pages, for a new wiki to be born out of
    /// the *page-group → wiki* regrouping pass: the LLM must find at least
    /// this many pages, from anywhere in the memory, that are the same
    /// subject area before any of them moves (`pages_to_new_wiki`).
    ///
    /// This is the page→folder rung of the forma fisica scale, above
    /// [`Self::auto_promote_min_page_facts`] (line→page), and the two do
    /// not compete: a page that has accumulated mass is *split into
    /// pages*, and only a **set** of pages that already exist becomes a
    /// wiki. A wiki is therefore never born with a single page, and the
    /// trigger is evidence on disk rather than a bet on future
    /// ramification.
    ///
    /// The floor governs **birth** only. Moving pages into a wiki that
    /// already exists (`pages_into_wiki`) has no floor — the home is
    /// already there, so a single stray page belongs inside just as much
    /// as nine do.
    pub auto_promote_group_min_pages: usize,
    /// Maximum number of page-merge candidate pairs the merge sub-job
    /// sends to the LLM confirmer per cycle (the cure front of semantic
    /// page consolidation — see the
    /// page-merge sub-job).
    /// A **resource** cap on confirmation calls, not a semantic gate —
    /// structural signals only nominate, the LLM decides. `0` disables
    /// the sub-job.
    pub page_merge_cap: usize,
    /// Pairs of near-duplicate topic words the night may put to the model.
    ///
    /// Counts the pairs that reach a JUDGEMENT, not the pairs the vector
    /// proposes: the vocabulary offers many and the night buys a few, richest
    /// first. Zero switches the sweep off.
    pub topic_merge_cap: usize,
    /// Maximum number of page moves the structural review applies per cycle.
    ///
    /// A **resource** cap on a costly, visible act, not a semantic gate: the
    /// judge is shown the whole forest and refuses freely, and this only
    /// bounds how much of one night's opinion is executed at once. Small on
    /// purpose — a page move rewrites paths and retargets links, and a night
    /// that moved twenty of them would be hard for anybody to read back.
    /// `0` disables the sub-job.
    pub structure_review_cap: usize,
    /// Maximum number of evidence facts the completion sweep sends to
    /// the LLM per cycle (the REM safety net behind the ingest closure
    /// verb).
    /// A **resource** cap: embedding similarity only nominates open
    /// candidates per evidence fact, the LLM decides what completed.
    /// `0` disables the sub-job.
    pub completion_sweep_cap: usize,
    /// Maximum number of candidate facts the cross-wiki refile sweep
    /// sends to the LLM per cycle (the LLM-decided refile of a single
    /// misfiled fact into a different existing wiki). A
    /// **resource** cap: a deterministic cosine pre-filter only nominates
    /// facts that embed materially closer to a foreign wiki than to home,
    /// the revisor LLM decides whether (and where) each really belongs.
    /// `0` disables the sub-job. Smart wikis are skipped as both source
    /// and destination (the ownership boundary is the consumer's).
    pub refile_sweep_cap: usize,
    /// How many under-linked pages the rail writer sends to the LLM per cycle.
    /// A **resource** cap like every other one here: which page needs a rail
    /// is deterministic, whether one is worth writing is the model's call.
    /// `0` disables the sub-job.
    pub rail_writer_cap: usize,
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
    /// How many pages the **page judge** may read in ONE NIGHT
    /// ([`JudgementDepth::Nightly`]).
    ///
    /// This is the budget in model calls, exactly: one page, one call,
    /// whatever the page holds.
    pub judgement_max_pages_night: usize,
    /// The same cap for the HOURLY pass ([`JudgementDepth::Hourly`]).
    ///
    /// Far lower, because the hour comes round twenty-four times to the
    /// night's once and reads what one round touched. What it leaves is not
    /// lost: the page is still unjudged, and the night reads it.
    pub judgement_max_pages_hour: usize,
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
    /// Max notifications the Briefing dispatcher emits per wiki
    /// per cycle — the global 50/h cap in [`crate::briefing`] backstops
    /// at the inbox level. Default 10.
    pub briefing_notify_cap: usize,
    /// Briefing dispatcher: a fact carrying `status: draft` (top-level
    /// YAML key) whose age exceeds this window triggers a stale-draft
    /// notify. Default 14 days.
    pub briefing_stale_draft_age: chrono::Duration,
    /// Briefing dispatcher: a fact whose `recall_count_30d` is at or
    /// above this threshold triggers a recall-hot notify suggesting the
    /// smart consumer promote it. Default 20.
    pub briefing_recall_hot_threshold: i64,
    /// Briefing dispatcher: the same
    /// `(wiki_id, source_ref)` is not re-emitted if a row already exists
    /// in `wiki_briefing_items` within this window. Default 7 days.
    pub briefing_dedup_window: chrono::Duration,
    /// Lease expirer: an active lease whose `expires_at` lies
    /// further than this grace period in the past is treated as
    /// crashed-without-release and marked `released_at = now`. Default
    /// 1 hour — slow clients about to re-acquire still win the race.
    pub lease_expirer_grace: chrono::Duration,
    /// Lease expirer: released rows older than this retention
    /// window are deleted. The `/dashboard/wiki/<id>/op-log` page reads
    /// the table for past leases, so the window doubles as the UI
    /// visibility budget. Default 7 days.
    pub lease_expirer_retention: chrono::Duration,
    /// Briefing-processor: master switch. `false`
    /// disables the sub-job for this cycle without forcing the
    /// operator to clear other policy fields. Default `true`.
    pub briefing_processor_enabled: bool,
    /// Briefing-processor: a row whose `ts` is
    /// within this grace period of `now` is left alone — the operator
    /// might still be editing the comment in the dashboard. "Mark as
    /// read" on the dashboard bypasses the grace period, the cycle does
    /// not. Default 15 minutes.
    pub briefing_processor_grace: chrono::Duration,
    /// Husk-page GC: page FILES removed per full cycle. A husk is a
    /// plan-absent, non-reserved page whose fact rows are all tombstoned
    /// or superseded — the files the compiler's orphan sweep keeps
    /// because a superseded row still points at them. Default 4; `0`
    /// disables.
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
            revisor_cap: 30,
            revisor_jaccard_min: 0.45,
            revisor_jaccard_max: recall::DEFAULT_DEDUP_THRESHOLD,
            revisor_cosine_min: 0.80,
            revisor_examined_cap: 120,
            auto_promote_cap: 5,
            auto_promote_min_page_facts: 8,
            auto_promote_min_page_facts_technical: 32,
            auto_promote_group_min_pages: 9,
            page_merge_cap: 3,
            topic_merge_cap: 4,
            structure_review_cap: 3,
            completion_sweep_cap: 8,
            refile_sweep_cap: 5,
            // One page per night is the shape this pass wants, not a sweep:
            // a rail is written into prose at the next rewrite, so a night
            // that adds twelve of them rewrites twelve pages, and a link the
            // model was unsure about costs a clause on every rewrite after.
            rail_writer_cap: 8,
            closure_sweep_window: chrono::Duration::hours(48),
            contradiction_sweep_cap: 8,
            date_normalize_cap: 16,
            judgement_max_pages_night: 30,
            judgement_max_pages_hour: 5,
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
///
/// `Default` is the report of a cycle that **did not run** — the same
/// idiom as [`crate::compiler::CompileReport`], which a skipped compile
/// returns empty rather than inventing a shape for "absent".
#[derive(Debug, Clone, Default)]
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
    /// Revisor sub-job report.
    pub revisor: RevisorReport,
    /// Auto-promote sub-job report.
    pub auto_promote: AutoPromoteReport,
    /// Page-merge sub-job report — LLM-confirmed consolidation of
    /// near-synonym concept pages (act-first, with a receipt).
    pub page_merge: PageMergeReport,
    /// Topic-word merge report — near-duplicate words the night folded into
    /// one, so a word counts for what it means rather than for how it was
    /// spelt that day.
    pub topic_merge: crate::topic_rank::TopicMergeReport,
    /// What the structural review moved — the one pass that looks at the
    /// whole forest, and the only one that can move a page between wikis.
    pub structure_review: StructureReviewReport,
    /// Completion sweep report — the REM safety net of the closure verb
    /// (closes open items whose completion ingest could not see).
    pub completion_sweep: CompletionSweepReport,
    /// What the page judge read and changed.
    pub page_judge: PageJudgementReport,
    /// Cross-wiki refile sweep report — moves single facts the revisor
    /// LLM deems misfiled into a different existing wiki (act-first,
    /// smart-skip).
    pub refile_sweep: RefileSweepReport,
    /// Contradiction sweep report — closes the satellites of a freshly
    /// contradicted fact that ingest could not see.
    pub contradiction_sweep: ContradictionSweepReport,
    /// Rail-writer report — the pass that decides a page should point
    /// somewhere, and parks the decision for the next rewrite to write.
    pub rail_writer: RailWriterReport,
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
    /// wiki owner's `_briefing.md` via [`crate::briefing::notify_as_rem`].
    pub briefing_dispatcher: BriefingDispatcherReport,
    /// Lease expirer sub-job report — prunes stale rows
    /// from `wiki_admin_leases`. Two passes: active-but-expired beyond
    /// grace get `released_at` stamped (treated as crashed without
    /// release), released rows beyond retention get deleted.
    pub lease_expirer: crate::wiki_admin_leases::ExpirerReport,
    /// Briefing-processor sub-job report —
    /// drains pending `wiki_briefing_items` rows on non-smart
    /// wikis past the grace period.
    pub briefing_processor: BriefingProcessorReport,
    /// Husk-page GC sub-job report — plan-absent page files whose rows
    /// are all tombstoned or superseded, removed from disk.
    pub husk_gc: HuskGcReport,
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
    /// confirmed pair merged **act-first** in-cycle.
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
/// plan re-homed, born-applied receipt). Silent: the nightly cycle does not
/// report its own housekeeping.
#[derive(Debug, Clone, Default)]
pub struct PageMergeReport {
    /// Candidate pairs that reached the LLM confirmation call.
    pub candidates_examined: usize,
    /// Pairs the LLM confirmed as the same concept.
    pub candidates_confirmed: usize,
    /// Born-applied receipt ids of executed merges.
    pub applied: Vec<String>,
    /// Pairs skipped because a page-merge receipt already covers them.
    pub skipped_judged: usize,
    /// Pairs skipped because the husk's `fact_index` rows were not all
    /// settled on its compiled page (pending renders) — retried on a
    /// later cycle once the compiler has caught up.
    pub skipped_unsettled: usize,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the structural review.
#[derive(Debug, Default, Clone)]
pub struct StructureReviewReport {
    /// Pages the inventory showed the judge.
    pub pages_shown: usize,
    /// Pages the cap left out — said out loud, never silently dropped, and
    /// told to the judge too so it knows its view is partial.
    pub pages_dropped: usize,
    /// Moves the judge named.
    pub moves_named: usize,
    /// Born-applied receipt ids of executed moves.
    pub applied: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// One page as the structural review is shown it.
struct ForestPage {
    /// `wiki_id/page-file`, the address the judge names back.
    address: String,
    /// The wiki it currently sits in.
    wiki_id: String,
    /// The page file within that wiki.
    page: String,
    /// Its card — the one line saying what belongs on it.
    card: String,
    /// How many facts live on it.
    facts: usize,
    /// `subject → count`, biggest first: who the page's facts are ABOUT.
    /// This is the evidence the judge weighs; the page's name is not.
    subjects: Vec<(String, usize)>,
    /// Whether the dominant subject differs from the wiki's own principal.
    /// A **nomination** signal only — it decides the order of the inventory
    /// under a cap, never whether a page is misplaced.
    off_principal: bool,
}

/// The judge's answer.
#[derive(Debug, serde::Deserialize)]
struct StructureDecision {
    #[serde(default)]
    moves: Vec<StructureMove>,
}

#[derive(Debug, serde::Deserialize)]
struct StructureMove {
    page: String,
    to_wiki: String,
    #[serde(default)]
    reason: String,
}

/// Sub-report for the page judge.
#[derive(Debug, Clone, Default)]
pub struct PageJudgementReport {
    /// Pages the judge actually read — one model call each.
    pub pages_read: usize,
    /// The source paths of those readings that LANDED: a page whose answer
    /// came back and could be read, even when every verdict in it was `keep`
    /// or the engine refused them all.
    ///
    /// A page whose call failed, or whose answer could not be read, is NOT
    /// here — nothing was decided about it, so the passes that stand aside for
    /// a reading must not stand aside for this one.
    pub pages_answered: Vec<String>,
    /// Pages the cap left out, oldest first. They are still unjudged, and the
    /// next pass — the next hour's, or tonight's — reads them.
    pub pages_left: usize,
    /// One line per change: `fact_id · verb` for a verdict about a fact, and
    /// for what the reading decided about the page itself, the page's own path
    /// or slug in front of `split` or `rail`.
    pub changed: Vec<String>,
    /// One line per verdict the engine put to a person instead of carrying
    /// out, `fact_id · proposal_id`. Only the identity card's own correction
    /// goes this way.
    pub asked: Vec<String>,
    /// One line per verdict the engine would not carry out, with the reason.
    pub refused: Vec<String>,
    /// Receipt ids: one per page that changed, plus the `rail_add` a parked
    /// link leaves — the same row the rail writer writes, so the Proposals page
    /// shows one kind of event whichever reading decided it.
    pub receipts: Vec<String>,
    /// Soft failures — a page that did not come back, a write that did not
    /// land. Never fatal to the cycle.
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
/// (born-applied receipt; the dashboard is where the operator reads it, and
/// nothing is pushed at anybody). Smart wikis are skipped as both source and
/// dest.
#[derive(Debug, Clone, Default)]
pub struct RefileSweepReport {
    /// Candidates the Revisore seeded on the parked plan (the
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
/// keeps a plan-absent file while ANY non-tombstoned row points at it;
/// this sweep removes the file once every remaining row is tombstoned or
/// superseded — the husks the delete/supersede machinery leaves
/// behind. Inbound links degrade to
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

/// Sub-report for the Briefing dispatcher.
///
/// Walks every wiki of the smart family looking for stale drafts and
/// recall-hot facts; per finding posts a single item to the wiki owner's
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

/// Sub-report for the Briefing-processor non-smart.
///
/// Drains `wiki_briefing_items` rows whose `wiki_id` is a
/// **non-smart** wiki (smart consumers maintain their own
/// smart wikis; REM maintains the standard families). Mark-passive
/// policy: stamp `processed_at = NOW()` after a pro-forma read of the
/// cited context. The same core function
/// ([`briefing_processor::process_briefing_item`]) is also invoked
/// synchronously from the dashboard's "Mark as read" button on a per-row
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
    /// world cause: somebody marked the row read in the dashboard
    /// between the candidate list and the per-row call.
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
    /// (near-duplicate of an existing same-subject fact — nothing inserted).
    pub facts_deduped: usize,
    /// Standard-wiki comments applied as fact ops: facts removed.
    pub facts_removed: usize,
    /// Standard-wiki comments applied as fact ops: facts moved to another
    /// page or wiki (born-applied — the `_direct` wrappers mint a
    /// receipt).
    pub facts_moved: usize,
    /// Ops a comment asked for that its author may not ask: `fact_id · action
    /// · reason`.
    ///
    /// The engine working, not failing — which is why it is not in
    /// [`Self::errors`]. The person who asked is told on a receipt of their
    /// own ([`crate::proposals::kind::COMMENT_REFUSED`]); this is the same
    /// thing in the night's own log, so an operator reading the cycle sees
    /// that a comment was partly turned down without going to look.
    pub comment_ops_refused: Vec<String>,
    /// Per-row soft errors (DB / filesystem / invalid `wiki_id` row).
    /// Hard failures bubble as [`RemError`]; everything else is
    /// collected here and the cycle keeps going.
    pub errors: Vec<String>,
}

/// Sub-report for the auto-apply sweep and the expire sweep that
/// follows it.
///
/// Walks every pending proposal past `timeout_at` and calls
/// [`crate::proposals::auto_apply_overdue_proposals`] which dispatches
/// to [`crate::proposals::auto_apply_proposal`] with the `recommended`
/// answers derived from the questionnaire.
#[derive(Debug, Clone, Default)]
pub struct AutoApplyReport {
    /// Rows the sweep loaded from `structure_proposals`.
    pub candidates_examined: usize,
    /// `(proposal_id, kind)` of the rows the sweep moved from
    /// `pending` to `applied`.
    pub applied: Vec<(String, String)>,
    /// Rows the expire sweep moved from `pending` to `expired` — the
    /// ones still failing once the grace window past `timeout_at`
    /// closed.
    pub expired: u64,
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
    /// applied — a new wiki born at the root, or pages filed into one
    /// that already existed.
    pub grouping_groups_applied: usize,
    /// Receipt ids of the structural changes **applied directly** this
    /// cycle (born-applied `wiki_promote` rows: `paragraph_to_file`,
    /// `pages_to_new_wiki` and `pages_into_wiki` share the
    /// `auto_promote_cap`). Each was announced with a `structure_applied`
    /// notice.
    pub applied: Vec<String>,
    /// Plan slugs of the pages a per-page split coined **this cycle**.
    ///
    /// A split page is, by construction, the same subject as the page it came
    /// out of — that is what splitting one means. The page-merge sub-job runs
    /// straight after this one and nominates pairs on **page-name kinship**,
    /// which the split target shares with its own source by design, so
    /// without this set the confirmer is handed the pair, answers "same
    /// concept" (it is), and puts back what was just taken apart. Measured
    /// 2026-08-25: a 27-fact clinical page split at 08:12:24 and was merged
    /// back at 08:12:26.
    ///
    /// The fence is **this cycle only**. A later night that finds the page
    /// genuinely duplicative is making a real judgement on evidence that has
    /// had time to accumulate, and it stays free to merge.
    pub split_targets: std::collections::BTreeSet<String>,
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
    /// `rem_dedup_semantic` slot — confirms suspicious dedup pairs.
    pub revisor: &'a dyn LlmBackend,
    /// `rem_promotions` slot — decides paragraph/file/wiki promotion.
    /// `None` ⇒ the slot has no model yet and the auto-promotion sub-job
    /// cannot run.
    pub auto_promote: Option<&'a dyn LlmBackend>,
    /// `ingest` slot — the cheap (Flash-tier) backend the **light** dream
    /// runs every compile stage on (tier-per-cadence: the strong
    /// model works only at REM, the light pass uses this slot via
    /// [`crate::dream`]). `None` ⇒ the light dream is left with whatever
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
    /// Smart-wiki section-index failure (the briefing dispatcher scanning a
    /// smart wiki's content).
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
    /// Archive-proposal emission failure (archive detector path).
    #[error("rem archive: {0}")]
    Archive(#[from] ArchiveError),
    /// Briefing inbox failure surfaced by the Briefing dispatcher when
    /// the notify pipeline raises
    /// an infrastructure-level error (sql / io). Per-finding soft errors
    /// (rate-limited inbox, invalid input on a synthesised row) are
    /// collected in the sub-job report rather than bubbling here.
    #[error("rem briefing: {0}")]
    Briefing(#[from] BriefingError),
    /// LLM call failed mid-cycle. This aborts
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
/// per-sub-job model handles ([`RemLlms`] names the slot each field is
/// wired to).
///
/// **The order of the seventeen sub-jobs is the order they are called in
/// below, and that call sequence is the only authority on it.** It is
/// load-bearing at two points: the auto-apply sweep settles overdue
/// proposals before any write-job touches the same rows, and provenance
/// hygiene runs immediately before the date normalizer so the normalizer
/// reads pointer-clean text.
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
    // smart wikis (per-wiki `_meta.md` flag). It decides where each sub-job
    // is allowed to work: the ones that rewrite compiled prose skip a smart
    // wiki, and the briefing dispatcher runs only there.
    let smart_wiki_index = load_smart_wiki_index(tree)?;

    // Expire aged confirmer memos before any sub-job reads them, so a
    // question whose TTL ran out is re-asked in THIS cycle rather than
    // the next one.
    let verdict_memo_purged =
        rem_verdicts::purge_older_than(pool, now - policy.verdict_memo_ttl).await?;

    // What the day did, read before any sub-job acts so nothing this cycle
    // writes counts as the day's. It orders the sweeps that ask a question
    // about a placement somebody just made; it never drops a candidate.
    let day = day::perimeter(pool).await;

    // **The two lists, and why they are two.** Tonight's reading is chosen once
    // and before anything reads a page, because passes below key on it — but on
    // two different questions, and they must not share one answer.
    //
    // `will_be_read` is the SELECTION: the pages the reading intends to open.
    // THREE passes run BEFORE the reading does and take this one, because the
    // selection is all that can be known when they choose: the completion and
    // contradiction sweeps, which stand aside only for a pair that lives
    // wholly on one of those pages, and the per-page split inside
    // [`run_auto_promote`], which stands aside for the page itself. Being
    // wrong costs each of them a night — a page whose reading then fails is
    // theirs again at the next round, and nothing is lost meanwhile.
    //
    // `were_read` is the READINGS THAT LANDED, filled in after the pass. The
    // passes that would carry a page to a model again key on that one: a page
    // whose reading failed answered nothing, so nobody may skip it on the
    // strength of a question that was never answered.
    //
    // A page the cap leaves out is on neither list, so every pass covers it.
    let fresh: Vec<FactId> = day
        .facts_written
        .iter()
        .filter_map(|id| FactId::parse(id).ok())
        .collect();
    let to_judge =
        pages_the_round_wrote_onto(pool, &fresh, JudgementDepth::Nightly.page_cap(policy)).await;
    let will_be_read: BTreeSet<&str> = to_judge.pages.iter().map(String::as_str).collect();

    let auto_apply = run_auto_apply_sweep(pool, tree, now).await?;
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
        &day,
        policy,
        &smart_wiki_index,
        &will_be_read,
    )
    .await?;
    let page_merge = run_page_merge(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        &day,
        policy,
        &smart_wiki_index,
        &auto_promote.split_targets,
        now,
    )
    .await?;
    // Word merging rides the same slot and the same act-first discipline as
    // the page merge above, and runs beside it for the same reason: both fold
    // two near-synonyms into one, one over pages and one over the words that
    // say what a fact is about. It touches no page and needs no compile —
    // topic words live in `fact_index` alone.
    let topic_merge = crate::topic_rank::merge_near_duplicates(
        pool,
        tree.workdir(),
        &embedder,
        llms.revisor,
        policy.topic_merge_cap,
    )
    .await
    .unwrap_or_else(|e| crate::topic_rank::TopicMergeReport {
        errors: vec![format!("topic merge: {e}")],
        ..Default::default()
    });

    // The forest review runs AFTER the passes that reshape a wiki's own
    // subtree and BEFORE the ones that move single facts: it should judge the
    // structure the night has already tidied, and a fact that is about to
    // follow its page should not be refiled on its own first.
    let structure_review = match llms.auto_promote {
        Some(strong) => run_structure_review(pool, tree, strong, policy)
            .await
            .unwrap_or_else(|e| StructureReviewReport {
                errors: vec![format!("structure: {e}")],
                ..StructureReviewReport::default()
            }),
        None => StructureReviewReport::default(),
    };
    let completion_sweep = run_completion_sweep(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        now,
        policy,
        &smart_wiki_index,
        &will_be_read,
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
        &will_be_read,
    )
    .await?;
    // The judge reads what the day WROTE, and it reads it after the two sweeps
    // that close things: a fact those already settled is not offered for a
    // second opinion, and a request the completion sweep finished is not closed
    // twice. Those sweeps stand aside the other way for a pair that lives
    // wholly on a page this reading will open
    // ([`all_on_one_page_the_reading_will_open`]), so the two never carry one
    // page to a model twice.
    //
    // **What runs beside what.** The sub-jobs of a cycle are sequential, and
    // they stay that way: nearly all of them write `fact_index` rows chosen by
    // what the rows say — the completion sweep closes a fact, the contradiction
    // sweep closes the same fact's neighbours, the judge closes both kinds —
    // so two of them at once would be two passes deciding the fate of one fact
    // from two readings taken before either wrote. What IS worth overlapping
    // is the waiting, and that happens inside a pass, where the questions are
    // known to be about different things: this one asks about several pages at
    // once and applies the answers one page at a time
    // ([`ask_about_every_page`]).
    //
    // It reads the PAGES the day wrote onto, each as compiled prose with its
    // facts marked, and asks of each everything that is decided by reading that
    // one page: the five about its facts, whether it has grown a second
    // subject, and which of its facts lead somewhere a reader cannot reach.
    // The pass runs on the `rem_promotions` slot — the one the night's other
    // judgements use. The hour has read some of these pages already and asked
    // three of the questions ([`JudgementDepth`]); this is the full reading,
    // and the shorter one does not settle it.
    //
    // **What stays separate, and why.** The completion and contradiction
    // sweeps above pair a fact with its nearest neighbours wherever they live,
    // and when one of those lives on another page this reading cannot see it —
    // so those pairs stay theirs. The page-group regrouping inside
    // [`run_auto_promote`] reads the memory as one shelf. What this reading
    // absorbs is what one page answers on its own.
    let page_judge = run_page_judgement(
        pool,
        tree,
        Arc::clone(&embedder),
        llms.auto_promote,
        &cycle_id,
        now,
        policy,
        &to_judge,
        JudgementDepth::Nightly,
    )
    .await?;
    let refile_sweep = run_refile_sweep(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        &day,
        policy,
        &smart_wiki_index,
    )
    .await?;
    // After the moves, before the compile: a rail is judged against where the
    // facts ended up tonight, and it is written into prose by the compile that
    // follows this cycle. The pages the judge read have answered this question
    // about themselves already; what is left for this pass is the under-linked
    // page nobody wrote onto, which is most of what it is for.
    let were_read: BTreeSet<&str> = page_judge
        .pages_answered
        .iter()
        .map(String::as_str)
        .collect();
    let rail_writer = run_rail_writer(
        pool,
        tree,
        llms.auto_promote,
        &cycle_id,
        &day,
        policy,
        &were_read,
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
    let husk_gc = run_husk_gc(pool, tree, &cycle_id, policy, &smart_wiki_index).await?;

    let ended_at = Utc::now();
    tracing::info!(
        cycle_id,
        auto_applied = auto_apply.applied.len(),
        auto_expired = auto_apply.expired,
        pairs_examined = revisor.pairs_examined,
        pairs_confirmed = revisor.pairs_confirmed,
        dedup_applied = revisor.applied.len(),
        promote_candidates = auto_promote.candidates_examined,
        grouping_wikis = auto_promote.grouping_wikis_examined,
        grouping_applied = auto_promote.grouping_groups_applied,
        promote_proposals = auto_promote.applied.len(),
        merge_candidates = page_merge.candidates_examined,
        merges_applied = page_merge.applied.len(),
        topic_pairs_judged = topic_merge.examined,
        topic_words_merged = topic_merge.merged.len(),
        completion_evidence = completion_sweep.evidence_examined,
        completions_closed = completion_sweep.closed.len(),
        refile_bridge_seeded = refile_sweep.bridge_candidates,
        refile_candidates = refile_sweep.candidates_examined,
        refiled = refile_sweep.refiled.len(),
        rails_nominated = rail_writer.nominated,
        rails_judged = rail_writer.judged,
        rails_written = rail_writer.written.len(),
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
        verdict_memo_purged,
        "rem: cycle done"
    );
    let verdict_memo_rows = rem_verdicts::count(pool).await?;
    Ok(RemCycleReport {
        cycle_id,
        started_at,
        ended_at,
        auto_apply,
        revisor,
        auto_promote,
        page_merge,
        topic_merge,
        structure_review,
        completion_sweep,
        page_judge,
        refile_sweep,
        contradiction_sweep,
        rail_writer,
        recall_repair,
        provenance_hygiene,
        date_normalizer,
        archive_detector,
        briefing_dispatcher,
        lease_expirer,
        briefing_processor,
        husk_gc,
        verdict_memo_purged,
        verdict_memo_rows,
    })
}

/// Sub-report for the rail writer.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RailWriterReport {
    /// Under-linked pages the pass nominated, before the cap and after the
    /// pages tonight's judge read are taken out of the list.
    pub nominated: usize,
    /// Pages that reached a model verdict.
    pub judged: usize,
    /// `(from, to)` rails the pass parked on the plan.
    pub written: Vec<(String, String)>,
    /// `(from, to)` rails it replaced to make room.
    pub replaced: Vec<(String, String)>,
    /// Born-applied `rail_add` receipt ids.
    pub receipts: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
    /// Why the pass did nothing at all, when it did nothing.
    pub disabled_reason: Option<String>,
}

/// One page's answer from the rail writer: a link per fact that needs one.
#[derive(Debug, Default, serde::Deserialize)]
struct RailDecision {
    #[serde(default)]
    links: Vec<RailChoice>,
    /// **Not part of the contract.** It is the key a prompt override asking
    /// for a single link produces, and it is read for one purpose: to say out
    /// loud that the override and this parser are asking different questions,
    /// rather than parking nothing and reading like a page that needed no
    /// rails. A single link could not be parked anyway — it names no fact, and
    /// a link that names no fact is discarded below.
    #[serde(default)]
    link: String,
}

/// One link the rail writer decided, and the fact it decided it for.
#[derive(Debug, Default, serde::Deserialize)]
struct RailChoice {
    #[serde(default)]
    link: String,
    /// The fact's 1-based number in the page block. A choice that names none
    /// is discarded: this pass answers a question about one fact, and an
    /// answer that cannot say which fact was not that answer.
    #[serde(default)]
    for_fact: Option<usize>,
    #[serde(default)]
    instead_of: Option<String>,
    #[serde(default)]
    why: String,
}

/// The rail writer — the pass that makes the founder's sentence true.
///
/// *«è dal lavoro del REM che si conta la bontà della memoria, perché i link
/// che il navigatore segue alla fine li ha decisi il REM»* (2026-08-04). Every
/// other link in this engine is decided by whoever is writing one page at a
/// time and never revisits the choice; this is the pass that looks at a page
/// from outside and reads its facts one at a time, asking of each what a
/// reader who has just met it would need and cannot reach from here.
///
/// **Who is nominated is shape, with measured evidence on top.** The shape is
/// a page carrying fewer than [`PAGE_RAIL_BUDGET`] links, fewest first: a page
/// with none at all leads, because it can only be reached by a search landing
/// on one of its own facts and from it a reader can go nowhere. Then
/// [`detect_missing_rails`] lifts the pages a reader opened beside another and
/// could not walk to — a gap that was measured, where "carries few links" is
/// only a shape. It reads `recall_log`, so it is silent on a memory nobody has
/// talked to yet, and that is why it orders the list instead of being it.
///
/// The count is a page's **own** links, because that is what a reader standing
/// on it can follow. A page ten others point at still leads nowhere, and is
/// nominated like any other: links have a direction and being pointed at is
/// not being connected.
///
/// **What it may link to comes from [`crate::candidates`]** — the same four
/// sources the placement and the writing stages use, so a `far` candidate
/// (one this page resembles in nothing) is in front of the model at all. A
/// nearest-N list would offer exactly the pages the linking rule says not to
/// link.
///
/// **It acts and leaves a receipt.** A rail is parked on the plan
/// ([`crate::planner::park_authored_rail`]) and becomes a mandatory
/// recommended link at that page's next rewrite: the decision becomes prose
/// the next time the page is written, and from then on the harvest reads it
/// off the page like any other.
///
/// **The pages tonight's reading ANSWERED are not nominated.** They were asked
/// this same question while it had the page open ([`RAILS_QUESTION`]), out of a
/// call somebody is paying for anyway. A page whose reading failed is not on
/// that list and is nominated here tonight, because nothing was decided about
/// it. What is left is what this pass is chiefly for: the under-linked page
/// nobody touched today, lifted by a reader who opened it beside another and
/// could not walk between them.
async fn run_rail_writer(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: Option<&dyn LlmBackend>,
    cycle_id: &str,
    day: &day::DayPerimeter,
    policy: &RemPolicy,
    were_read: &BTreeSet<&str>,
) -> Result<RailWriterReport> {
    let mut report = RailWriterReport::default();
    if policy.rail_writer_cap == 0 {
        report.disabled_reason = Some("rail_writer_cap is 0".to_owned());
        return Ok(report);
    }
    let Some(llm) = llm else {
        report.disabled_reason = Some("no rem_promotions LLM wired".to_owned());
        return Ok(report);
    };
    // A plan that cannot be read is a night without this pass, never a night
    // that fails: nothing here is a repair, only an addition.
    let plan = match crate::planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        Ok(None) => {
            report.disabled_reason = Some("no plan yet".to_owned());
            return Ok(report);
        },
        Err(e) => {
            report.disabled_reason = Some(format!("plan unreadable: {e}"));
            return Ok(report);
        },
    };

    // Under-linked pages, fewest links first, then what the day touched, then
    // the slug so two runs over one plan agree.
    let mut nominees: Vec<&str> = plan
        .pages
        .iter()
        .filter(|(_, p)| !p.primary_facts.is_empty())
        .filter(|(_, p)| !crate::wiki::names_reserved_page(std::path::Path::new(&p.page_path)))
        .filter(|(slug, _)| plan.link_graph.get(*slug).map_or(0, Vec::len) < PAGE_RAIL_BUDGET)
        // A page tonight's judge read was asked this there, in the reading it
        // was already paying for.
        .filter(|(_, p)| {
            crate::planner::plan_page_source_path(tree, p)
                .is_none_or(|path| !were_read.contains(path.as_str()))
        })
        .map(|(slug, _)| slug.as_str())
        .collect();
    drop_the_pages_that_already_answered(pool, &plan, llm.model_id(), &mut nominees).await?;

    nominees.sort_by(|a, b| {
        let n = |s: &str| plan.link_graph.get(s).map_or(0, Vec::len);
        n(a).cmp(&n(b))
            .then_with(|| day.touched_page(b).cmp(&day.touched_page(a)))
            .then_with(|| a.cmp(b))
    });
    // Behavioural evidence first, where there is any: a page a reader opened
    // beside another and could not walk to it from is a measured gap, where
    // "carries few links" is only a shape. It is silent on a memory nobody has
    // talked to yet, which is why it leads the list instead of being it.
    //
    // Only the page the reader was standing on is lifted. The one at the far
    // end may already point here, and pointing back is not owed.
    let measured: BTreeSet<String> =
        match detect_missing_rails(pool, tree, &plan, 2, policy.rail_writer_cap).await {
            Ok(gaps) => gaps.into_iter().map(|r| r.from_slug).collect(),
            Err(e) => {
                report
                    .errors
                    .push(format!("rails: co-open evidence unread: {e}"));
                BTreeSet::new()
            },
        };
    if !measured.is_empty() {
        nominees.sort_by_key(|s| !measured.contains(*s));
    }
    report.nominated = nominees.len();
    nominees.truncate(policy.rail_writer_cap);
    if nominees.is_empty() {
        return Ok(report);
    }

    let candidates = the_candidate_pool(pool, tree, &plan).await;
    for slug in nominees {
        match judge_rails(pool, tree, llm, cycle_id, &plan, &candidates, slug).await {
            // One page, one judgement, however many links came back — a page
            // the model looked at and left alone was still judged.
            Ok(outcomes) => {
                report.judged += 1;
                for outcome in outcomes {
                    report.written.push((slug.to_owned(), outcome.to));
                    if let Some(dropped) = outcome.replaced {
                        report.replaced.push((slug.to_owned(), dropped));
                    }
                    if let Some(receipt) = outcome.receipt {
                        report.receipts.push(receipt);
                    }
                }
            },
            Err(e) => report.errors.push(format!("rail {slug}: {e}")),
        }
    }
    Ok(report)
}

/// The pool a whole pass picks its destinations out of, read once.
///
/// An identity card is not in it: the compiler refuses one as a rail
/// (`compiler::recommended_link_targets`), so offering it would spend a model
/// call and one of the page's link slots on a choice that is dropped before
/// the page is written.
async fn the_candidate_pool(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
) -> crate::candidates::CandidatePool {
    let by_source_path: BTreeMap<String, String> = plan
        .pages
        .iter()
        .filter(|(_, p)| !p.is_identity_card())
        .filter_map(|(slug, p)| {
            Some((
                crate::planner::plan_page_source_path(tree, p)?,
                slug.clone(),
            ))
        })
        .collect();
    crate::candidates::CandidatePool::load(pool, &by_source_path).await
}

/// Take out the pages that have already said they need no link.
///
/// A page answers this question in whichever reading asks it, and the answer
/// is remembered against the page's own facts ([`rails_memo_subject`]).
/// Without this the night's other reading answers «no link needed» and this
/// pass buys the same question again tomorrow, on a page nothing has touched.
///
/// # Errors
///
/// Surfaces the memo read.
async fn drop_the_pages_that_already_answered(
    pool: &SqlitePool,
    plan: &CompilationPlan,
    model_id: &str,
    nominees: &mut Vec<&str>,
) -> Result<()> {
    let mut settled: BTreeSet<&str> = BTreeSet::new();
    for slug in nominees.iter() {
        if let Some(page) = plan.pages.get(*slug) {
            let key = rem_verdicts::key(model_id, &rails_memo_subject(slug, page));
            if rem_verdicts::is_settled(pool, rem_verdicts::kind::RAIL, &key).await? {
                settled.insert(*slug);
            }
        }
    }
    nominees.retain(|slug| !settled.contains(slug));
    Ok(())
}

/// What the model is shown about one under-linked page: its card, its facts
/// NUMBERED (the answer names one of those numbers), the links it carries,
/// the candidate destinations, and how many links it still has room for.
///
/// The links it already carries are **labelled by who wrote them**, because
/// that decides what the model may do with them: a rail this pass wrote before
/// may be replaced, a link the page's own prose carries may not — it belongs
/// to whoever wrote it — the Cronista, or whatever else touched the file.
fn rail_prompt(
    tree: &WikiTree,
    plan: &CompilationPlan,
    slug: &str,
    page: &crate::planner::PagePlan,
    offered: &[crate::candidates::Candidate],
    mine: &BTreeSet<&str>,
    budget: usize,
) -> Result<String> {
    let links = plan
        .link_graph
        .get(slug)
        .filter(|l| !l.is_empty())
        .map_or_else(
            || "none".to_owned(),
            |ls| {
                ls.iter()
                    .map(|l| {
                        let tag = if mine.contains(l.as_str()) {
                            " (written by this pass — may be replaced)"
                        } else {
                            " (written into the prose — not yours to remove)"
                        };
                        format!("- {l}{tag}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        );
    // Numbered, because the answer names one: `for_fact` is an index into
    // exactly this list, so the rendering and the parsing share one origin.
    let facts = page
        .primary_facts
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}. {}", i + 1, f.text.replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");
    let page_block = format!(
        "{slug} — {}\n  what belongs on it: {}\n  its facts:\n{facts}",
        page.title, page.description
    );
    let candidates_block = offered
        .iter()
        .filter_map(|c| {
            let p = plan.pages.get(&c.key)?;
            Some(format!(
                "- [{}] {}: {}",
                c.source.tag(),
                c.key,
                p.description
            ))
        })
        .collect::<Vec<_>>()
        .join("\n");
    prompts::render(
        "rem-rails",
        tree.workdir(),
        BUNDLED_REM_RAILS_MD,
        &[
            ("page", page_block.as_str()),
            ("links", links.as_str()),
            ("candidates", candidates_block.as_str()),
            ("budget", budget.to_string().as_str()),
        ],
    )
    .map_err(RemError::from)
}

/// What one accepted rail did.
struct RailOutcome {
    to: String,
    replaced: Option<String>,
    /// `None` when the rail was parked but its receipt could not be written —
    /// the park stands, so the pass reports the rail and not a receipt id it
    /// does not have.
    receipt: Option<String>,
}

/// The destinations offered for one page, ranked on its card AND its facts.
///
/// The card vector says what the page is ABOUT, and a selection ranked on it
/// offers the neighbourhood of that theme. But the question this pass asks is
/// per fact — what does somebody who has just read THIS need next — and the
/// answer often lives nowhere near the page's own centre: a week's page holds
/// school hours whose neighbour is a page of afternoon courses, and the two
/// themes resemble each other in nothing. Ranked on the card alone that
/// candidate reaches the model only if the `far` sample happens to draw it.
///
/// Scoring is max-over-the-bag, so widening with the facts is strictly
/// additive: every page the card alone would have offered is still offered. A
/// fact the embedder never reached contributes nothing and costs nothing.
async fn rail_candidates(
    pool: &SqlitePool,
    plan: &CompilationPlan,
    candidates: &crate::candidates::CandidatePool,
    slug: &str,
    page: &crate::planner::PagePlan,
) -> Vec<crate::candidates::Candidate> {
    let exclude: BTreeSet<String> = std::iter::once(slug.to_owned())
        .chain(plan.link_graph.get(slug).into_iter().flatten().cloned())
        .collect();
    // The fallback, for a page `page_card` has no vector for — a page born
    // this cycle, or a corpus whose cards have never been embedded. Its own
    // wiki, heaviest first: without it the pass would do nothing at all on a
    // fresh memory, which is the memory that most needs its first rails.
    let mut home: Vec<&crate::planner::PagePlan> = plan
        .pages
        .values()
        .filter(|p| p.wiki_id == page.wiki_id && p.slug != slug)
        // The fence the pool applies, applied to the fallback too: a card is
        // not a destination, and this list bypasses the pool entirely.
        .filter(|p| !p.is_identity_card())
        .collect();
    home.sort_by_key(|p| std::cmp::Reverse(p.primary_facts.len()));
    let home: Vec<String> = home.into_iter().map(|p| p.slug.clone()).collect();

    let mut ask = candidates.ask_for([slug]);
    let fact_ids: Vec<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();
    match crate::fact_index::embeddings_of(pool, &fact_ids).await {
        Ok(vs) => ask.widen_with(vs),
        Err(e) => tracing::warn!(slug, error = %e,
            "rails: fact vectors unread — ranking on the page's card alone"),
    }
    candidates.pick(&ask, &exclude, crate::candidates::SELECTION_PAGES, &home)
}

/// The destination and the fact one choice names, or `None` when a fence drops it.
///
/// Three fences, each dropping a single choice rather than the whole answer —
/// one bad line does not cost the good ones beside it:
///
/// - the destination must be a page the model was **offered**, so a name it
///   invented reaches nothing;
/// - two choices naming one destination are one rail, and the second would
///   park nothing while spending a receipt to say so;
/// - `for_fact` must be a number in this page's own list, because the pass
///   answers a question about one fact and an answer that cannot say which
///   fact was not that answer.
fn accepted_choice<'c>(
    choice: &'c RailChoice,
    page: &crate::planner::PagePlan,
    offered: &[crate::candidates::Candidate],
    taken: &mut BTreeSet<String>,
    slug: &str,
) -> Option<(&'c str, String)> {
    let to = choice.link.trim();
    if to.is_empty() || to.eq_ignore_ascii_case("none") {
        return None;
    }
    if !offered.iter().any(|c| c.key == to) {
        tracing::warn!(
            slug,
            to,
            "rem rails: model named a page it was not offered — skipped"
        );
        return None;
    }
    if !taken.insert(to.to_owned()) {
        return None;
    }
    let Some(for_fact) = choice
        .for_fact
        .filter(|n| *n >= 1 && *n <= page.primary_facts.len())
        .map(|n| page.primary_facts[n - 1].fact_id.as_str().to_owned())
    else {
        tracing::warn!(
            slug,
            to,
            "rem rails: a link naming no fact of this page — skipped"
        );
        return None;
    };
    Some((to, for_fact))
}

/// What the memo remembers when a reading answers «nowhere» about a page's
/// links, keyed so BOTH readings compute the same string: the page, and the
/// content the answer was about.
///
/// **Why the page's own facts and not the destinations offered.** The question
/// is about a NEED — does a fact here leave a reader with something this page
/// cannot answer — and the answer to that changes when the page's facts
/// change, not when a new neighbour appears somewhere else. So a page that has
/// answered «nowhere» is not asked again until it says something different.
///
/// **What a fact says includes when it stopped being true.** Each one is
/// written down the way [`JudgedPage::memo_subject`] writes it — id, kind, end,
/// words — because a page that loses half its facts to a closure is a page
/// saying something different, and a key that read only the words would hold
/// the old answer until somebody typed a new sentence. The end comes from the
/// plan, so a closure reaches this key at the compile that follows it, which
/// is the same moment the prose itself catches up.
fn rails_memo_subject(slug: &str, page: &crate::planner::PagePlan) -> String {
    use std::fmt::Write as _;

    let mut out = format!("rails|{slug}");
    for f in &page.primary_facts {
        let _ = write!(
            out,
            "|{}:{}:{}:{}",
            f.fact_id.as_str(),
            f.fact_type.as_deref().unwrap_or(""),
            f.valid_to.as_deref().unwrap_or(""),
            f.text,
        );
    }
    out
}

/// Park one accepted rail and leave its receipt. `None` when the park refused.
///
/// The rail is parked first, so a receipt failure is reported and survived
/// rather than raised: undoing the park to keep the paper trail tidy would
/// throw away the decision the model was called to make. The fact the rail was
/// written for rides the receipt — the park itself carries `(from, to)` and
/// nothing more.
async fn park_one_rail(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    from: &str,
    to: &str,
    for_fact: Option<&str>,
    replaced: Option<String>,
    why: &str,
) -> Option<RailOutcome> {
    match crate::planner::park_authored_rail(tree, from, to, replaced.as_deref()) {
        Ok(true) => {},
        Ok(false) => return None,
        Err(e) => {
            tracing::warn!(from, to, error = %e, "rem rails: rail not parked");
            return None;
        },
    }
    let context = serde_json::json!({
        "from": from,
        "to": to,
        "for_fact": for_fact,
        "instead_of": replaced,
        "why": why,
        "cycle_id": cycle_id,
    });
    let params = crate::proposals::EmitParams::new(
        crate::proposals::kind::RAIL_ADD,
        context.clone(),
        serde_json::json!([]),
    );
    let receipt =
        match crate::proposals::emit_applied_proposal(pool, params, context, Some("rem")).await {
            Ok(e) => Some(e.proposal_id),
            Err(e) => {
                tracing::warn!(
                    from,
                    to,
                    error = %e,
                    "rem rails: the rail is parked, the receipt is not recorded"
                );
                None
            },
        };
    Some(RailOutcome {
        to: to.to_owned(),
        replaced,
        receipt,
    })
}

/// Ask the model which of `slug`'s facts needs a neighbour, and park what it says.
///
/// An empty answer is the common one and not a failure: a link nobody needs
/// costs a clause of prose on every future rewrite of the page.
///
/// Three fences stand between the reply and the plan, and each drops a single
/// choice rather than the whole answer — one bad line does not cost the good
/// ones beside it:
///
/// - the destination must be a page the model was **offered**, so a name it
///   invented reaches nothing;
/// - `for_fact` must be a number in the page's own list, because this pass
///   answers a question about one fact and an answer that cannot say which
///   fact was not that answer;
/// - the page's remaining room (`PAGE_RAIL_BUDGET` minus what it carries)
///   caps how many are parked, so one night can fill a page but not flood it.
async fn judge_rails(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    plan: &CompilationPlan,
    candidates: &crate::candidates::CandidatePool,
    slug: &str,
) -> Result<Vec<RailOutcome>> {
    let Some(page) = plan.pages.get(slug) else {
        return Ok(Vec::new());
    };
    let carried = plan.link_graph.get(slug).map_or(0, Vec::len);
    let budget = PAGE_RAIL_BUDGET.saturating_sub(carried);
    if budget == 0 {
        return Ok(Vec::new());
    }
    let offered = rail_candidates(pool, plan, candidates, slug, page).await;
    if offered.is_empty() {
        return Ok(Vec::new());
    }

    let mine: BTreeSet<&str> = plan
        .authored_rails
        .iter()
        .filter(|(a, _)| a == slug)
        .map(|(_, b)| b.as_str())
        .collect();
    let prompt = rail_prompt(tree, plan, slug, page, &offered, &mine, budget)?;
    // A page whose neighbourhood has not changed gets the same answer, so a
    // byte-identical re-ask is the most expensive no-op in the cycle.
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::RAIL, &memo_key).await? {
        return Ok(Vec::new());
    }

    let resp = llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.2)
                .with_max_tokens(700),
        )
        .await
        .map_err(|e| RemError::Llm(format!("rail writer failed on {slug}: {e}")))?;
    let decision: RailDecision = first_json_object(&resp.text)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    if decision.links.is_empty() && !decision.link.trim().is_empty() {
        tracing::warn!(
            slug,
            "rem rails: the reply carries a single `link` and this pass reads `links` \
             — the prompt override in the workdir asks a different question from the \
             one the code parses"
        );
    }

    let mut out: Vec<RailOutcome> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for choice in decision.links {
        if out.len() == budget {
            break;
        }
        let Some((to, for_fact)) = accepted_choice(&choice, page, &offered, &mut taken, slug)
        else {
            continue;
        };
        // The swap half, and its fence: only a rail this pass wrote may go.
        let replaced = choice
            .instead_of
            .as_deref()
            .map(str::trim)
            .filter(|d| mine.contains(d))
            .map(ToOwned::to_owned);
        if let Some(outcome) = park_one_rail(
            pool,
            tree,
            cycle_id,
            slug,
            to,
            Some(&for_fact),
            replaced,
            &choice.why,
        )
        .await
        {
            out.push(outcome);
        }
    }
    if out.is_empty() {
        rem_verdicts::record_negative(pool, rem_verdicts::kind::RAIL, &memo_key, slug).await?;
    }
    Ok(out)
}

// ---------- Smart family index ----------

/// Cycle-scoped cache of `wiki_id -> smart`. One tree walk reading
/// the per-wiki smart flag from each `_meta.md`. The 5 write-jobs
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
/// like a non-smart standard wiki). This keeps the write-jobs
/// working on partially-broken trees rather than silently
/// dropping work.
fn is_smart_wiki(smart_wiki_index: &SmartWikiIndex, wiki_id: &str) -> bool {
    smart_wiki_index.get(wiki_id).copied().unwrap_or(false)
}

// ---------- Consolidation scopes ----------

/// One consolidation scope: a standard wiki.
///
/// The consolidation passes (dedup revisor, completion sweep, contradiction
/// sweep, page-merge) pool their candidates per scope, and a scope is one
/// wiki: a standard wiki hangs under nothing, so there is nothing else to
/// pool with. Fragments of one subject scattered over several wikis are the
/// page-group grouping's business — it gathers them into one wiki, and they
/// meet here afterwards. Arbitrary cross-wiki pairs stay out of scope
/// (self-correcting REM's future business); smart wikis are excluded
/// entirely, as every pass already skips them.
struct ConsolidationScope {
    /// The wiki this scope is.
    wiki_id: String,
    /// Whether this wiki is an AGENT's own memory — its autobiography rather
    /// than a person's. The confirmer sweeps switch rubric on it, and
    /// resolving it here means they get it for free instead of re-locating a
    /// wiki per candidate pair.
    is_agent: bool,
}

/// Every standard wiki, as its own consolidation scope.
fn consolidation_scopes(
    tree: &WikiTree,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<Vec<ConsolidationScope>> {
    Ok(tree
        .walk()?
        .into_iter()
        .filter(|d| !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()))
        .map(|d| ConsolidationScope {
            wiki_id: d.meta.wiki_id.as_str().to_owned(),
            is_agent: d.meta.is_agent,
        })
        .collect())
}

/// `wiki_id` → whether it is an agent's own memory.
///
/// The confirmer sweeps judge one case at a time, so they need the rubric
/// switch keyed by the wiki a case came from rather than by scope.
fn agent_wikis(scopes: &[ConsolidationScope]) -> BTreeMap<String, bool> {
    scopes
        .iter()
        .map(|s| (s.wiki_id.clone(), s.is_agent))
        .collect()
}

/// The wikis a pair may be nominated from — every standard one.
fn consolidating_wikis(scopes: &[ConsolidationScope]) -> BTreeSet<String> {
    scopes.iter().map(|s| s.wiki_id.clone()).collect()
}

// ---------- Auto-apply sweep sub-job ----------

/// The two overdue-proposal sweeps, adapted to the REM sub-job report
/// shape ([`AutoApplyReport`]).
///
/// First [`proposals::auto_apply_overdue_proposals`] flips
/// `pending → applied` with `apply_mode='auto'`; per-row handler
/// failures are collected and the sweep keeps going, leaving the row
/// `pending` for another night. Then
/// [`proposals::expire_overdue_proposals`] flips
/// `pending → expired` for the rows still overdue past
/// [`proposals::EXPIRE_GRACE_PERIOD`], which is what stops a row that
/// fails every night from being retried forever. Both get the cycle's
/// `now`, so the grace window is measured against one clock.
async fn run_auto_apply_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    now: DateTime<Utc>,
) -> Result<AutoApplyReport> {
    let sweep = proposals::auto_apply_overdue_proposals(pool, tree, now).await?;
    let expire = proposals::expire_overdue_proposals(pool, now).await?;
    Ok(AutoApplyReport {
        candidates_examined: sweep.candidates_examined,
        applied: sweep.auto_applied,
        expired: expire.expired,
        errors: sweep.errors,
    })
}

// ---------- Shared LLM-failure discipline ----------

/// Consecutive failed model calls that mean the backend is down rather than
/// one reply being malformed.
///
/// Below it a sub-job records the failure, skips that candidate and carries
/// on; at it it gives up, with the disposition its own call site documents.
/// Each sub-job counts its own run and resets the count on the first
/// success.
const LLM_FAILURE_ABORT: usize = 5;

/// Record one failed model call and say whether the run of them is long
/// enough to give up.
///
/// A REM sub-job that turns a transport error into an `Err` costs the whole
/// night: `run_cycle` propagates it and [`crate::dream::run_full`] then
/// skips the compile and the closing pass, so the captures queued today are
/// not drained and the retry is a day away. One flaky reply must therefore
/// cost its candidate and nothing more — the candidate stays nominable, with
/// no verdict memoised, and the next cycle asks again.
///
/// A backend that is actually down is the other case, and
/// [`LLM_FAILURE_ABORT`] separates them. What that costs is the caller's
/// call: the structural sub-jobs return the partial report they have built
/// and the night continues into whatever is behind them, while the revisor
/// takes the cycle with it and says there why.
fn note_llm_failure(errors: &mut Vec<String>, consecutive: &mut usize, note: String) -> bool {
    *consecutive += 1;
    let stop = *consecutive >= LLM_FAILURE_ABORT;
    if stop {
        tracing::warn!(
            consecutive = *consecutive,
            note,
            "rem: consecutive model-call failures reached the abort threshold"
        );
    } else {
        tracing::warn!(note, "rem: candidate skipped, the cycle continues");
    }
    errors.push(note);
    stop
}

// ---------- Revisor jaccard semantic sub-job ----------

#[allow(
    clippy::too_many_lines,
    reason = "pairwise pre-pass + LLM confirm + proposal emit live as one loop on purpose"
)]
/// Do these two facts speak to different audiences?
///
/// The revisor's **audience gate**, and the reason it is a structural
/// invariant rather than a prompt instruction: same content is not the same
/// fact. A claim that reached two people by two private routes, each holding
/// it privately, is two facts — merging them retires one principal's memory
/// and leaves the survivor addressing the other's readers, which hands
/// somebody something they were never told and cannot be undone once the
/// loser's bytes are off the page (founder, 2026-07-28). Text similarity is a
/// candidate signal; the audience decides.
///
/// The reader set comes from [`crate::acl::reader_set`], beside `can_read`
/// itself, so this question and the one the read path asks every turn cannot
/// drift apart. Group rosters are deliberately not expanded there: two facts
/// naming different groups are two audiences even when today's membership
/// happens to coincide.
fn reader_sets_differ(a: &fact_index::FactIndexRow, b: &fact_index::FactIndexRow) -> bool {
    crate::acl::reader_set(&a.subject_id, &a.allow_ids, a.sender_id.as_ref())
        != crate::acl::reader_set(&b.subject_id, &b.allow_ids, b.sender_id.as_ref())
}

#[allow(
    clippy::too_many_lines,
    reason = "the pairing loop is one screen of guards in a load-bearing order — channel, identity core, audience, similarity — and splitting it hides which runs before the LLM"
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
    // Consecutive confirm failures — the outage detector behind the
    // per-pair skip below.
    let mut llm_failures: usize = 0;
    // One scope per standard wiki. Smart wikis are out entirely — the smart
    // consumer owns those writes via `wiki_admin_push`, REM never dedups them.
    for scope in consolidation_scopes(tree, smart_wiki_index)? {
        if report.applied.len() >= policy.revisor_cap || examined_capped {
            break;
        }
        let facts = fact_index::find_active_in_wiki(pool, &scope.wiki_id).await?;
        if facts.len() < 2 {
            continue;
        }
        let ngrams: Vec<HashSet<String>> = facts
            .iter()
            .map(|f| recall::ngrams(&f.text, recall::DEFAULT_NGRAM))
            .collect();
        // Channel-page membership per fact: dedup pairs never cross the
        // boundary (both sides on a reserved channel page, or neither).
        // Whether each fact's validity window is shut. On the rules page that
        // means the directive was taken back: the channel already refuses to
        // serve it, and it must not be weighed against the ones in force.
        let window_is_shut: Vec<bool> = facts.iter().map(|f| f.valid_to.is_some()).collect();
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
                // `@rules.md` per wiki). A pair mixing a rule with an
                // ordinary fact is never nominated: if the rule lost, its
                // content would survive only OFF `@rules.md`, out of the
                // behaviour-rules channel — the dedup twin of the compiler
                // and refile skips. A structural channel invariant, not a
                // semantic gate: rule-vs-rule pairs still go to the LLM.
                if on_channel_page[new_idx] != on_channel_page[old_idx] {
                    continue;
                }
                // A WITHDRAWN rule is nobody's twin. Closing a directive's
                // window is how a person takes it back, and the channel stops
                // serving it at once — but the row stays live and stays on the
                // page (cutting its region would hand it to the orphan sweep,
                // which tombstones a marker-less active row), so it arrives
                // here like any other. Paired against a rule in force it can
                // retire one that IS being obeyed, on the strength of words
                // nobody obeys.
                if on_channel_page[new_idx] && (window_is_shut[new_idx] || window_is_shut[old_idx])
                {
                    continue;
                }
                // Identity-core stickiness: background dedup never retires a
                // fact from the subject's always-on identity core (role /
                // relationship / bio, `salience=high`). The loser is
                // `facts[old_idx]` (the pair sorts newest-first, older side
                // retired); if that is an identity-core fact, skip the pair so
                // a relationship like "Frodo is Galadriel's partner" is
                // changed only by an explicit correction, never silently
                // consolidated away. A structural channel invariant, same
                // shape as the rules-page guard above — the LLM never sees it.
                if facts[old_idx].is_identity_core() {
                    continue;
                }
                // 🚨 THE AUDIENCE GATE. Two facts carrying the same content
                // are not necessarily one fact: a claim that reached two
                // people by two private routes, each holding it privately,
                // stays TWO facts. Merging them retires one principal's
                // memory and leaves the survivor speaking to the other's
                // readers — it hands somebody something they were never told,
                // and the loser's bytes are gone from the page, so there is no
                // undo. Text similarity is a candidate signal, never a
                // sufficient one: the audience and the provenance decide
                // (founder, 2026-07-28).
                //
                // Structural, and placed with the other invariants **before**
                // the LLM sees the pair — a rule the model could weigh is a
                // rule that fails on the day it matters. Costs nothing: the
                // three fields are already on the rows the loop holds.
                if reader_sets_differ(&facts[new_idx], &facts[old_idx]) {
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
                let prompt =
                    revisor_prompt(tree, &facts[new_idx], &facts[old_idx], scope.is_agent)?;
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
                // One flaky reply ("gemini response has no `text` part")
                // costs the pair and not the night: it stays nominable next
                // cycle, unrecorded, exactly as the completion and
                // contradiction confirmers already do ([`note_llm_failure`]).
                // Where the other sub-jobs stop themselves, an outage here
                // aborts the whole cycle — the promote, the merge and the
                // compile read this same slot, so none of them would work
                // either.
                let resp = match llm
                    .complete(
                        CompletionRequest::new(prompt)
                            .with_temperature(0.1)
                            .with_max_tokens(60),
                    )
                    .await
                {
                    Ok(r) => {
                        llm_failures = 0;
                        r
                    },
                    Err(e) => {
                        let note = format!(
                            "revisor failed on pair ({}, {}): {e}",
                            facts[new_idx].fact_id.as_str(),
                            facts[old_idx].fact_id.as_str()
                        );
                        if note_llm_failure(&mut report.errors, &mut llm_failures, note.clone()) {
                            return Err(RemError::Llm(format!(
                                "{note} ({llm_failures} consecutive revisor failures — backend down)"
                            )));
                        }
                        continue;
                    },
                };
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
                    Some(scope.wiki_id.as_str()),
                    None,
                )
                .await?;
                // 0032: address the merge receipt to the winner fact's
                // human (the survivor is the one that stays on the page).
                let recipient = proposals::recipient_from_fact(
                    &facts[new_idx].subject_id,
                    facts[new_idx].sender_id.as_ref(),
                );
                let hints = DedupMergeHints {
                    jaccard: Some(score),
                    // The winner's own wiki: the survivor stays where it
                    // lives.
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

/// The extra rubric line handed to the dedup confirmer when the pair lives in
/// an **agent's own** wiki.
///
/// The default rubric resolves subject elisions against each fact's page and
/// then judges the claims — right for a person's memory, wrong for an agent's
/// autobiography, where the *person the episode was lived with* is part of the
/// fact. Two near-identical sentences about two different users are two
/// memories of two relationships, and folding them would leave the agent
/// remembering one of them as if it had happened with the other (the founder's
/// 2026-07-28 ruling: dedup weighs audience and provenance, never wording
/// alone). Empty for every other family, so the ordinary rubric is unchanged.
const AGENT_DEDUP_NOTE: &str = "AGENT AUTOBIOGRAPHY — these facts live in an AI agent's OWN wiki: \
its memory of what it did, learned and became, one thread per person it works with. Here WHO the \
episode was lived with is part of the fact. Two statements that are worded almost identically but \
concern DIFFERENT people, or reached the agent through different exchanges, are DIFFERENT \
memories — answer {\"same\": false}. Answer {\"same\": true} only for a genuine restatement of the \
SAME episode or trait with the same person.";

/// The same rubric switch for the **completion** confirmer.
///
/// An agent's wiki is a log of service: "I advised", "I explained", "I sent
/// the label to print". Read as a person's memory those sentences look like
/// evidence that the person's intention was spent — the sweep would close
/// "she must buy the peri bottle" on "the agent recommended the peri bottle".
/// The generic rubric already says advising never completes; here it is the
/// dominant shape of the corpus, so it is spelled out for the subject.
const AGENT_COMPLETION_NOTE: &str = "AGENT AUTOBIOGRAPHY — these facts are an AI agent's OWN \
memory, so most of them narrate what the AGENT did for someone: advised, explained, compared, \
looked up, printed. Helping with an item NEVER completes it — only the person actually doing, \
buying or receiving the thing does. An open item of the agent's own (something it undertook to \
do) closes only on evidence the agent DELIVERED it, never on evidence it discussed it again.";

/// The same rubric switch for the **page-merge** confirmer.
///
/// The agent's diary is one page per served person (`esperienze_<user>`,
/// the founder's choice), and `slug_kinship` nominates exactly that
/// shape as a merge pair — two slugs sharing the `esperienze` token. Merging
/// them would collapse the threads the split exists to keep apart, so the
/// confirmer is told the per-person page IS the organising principle here.
const AGENT_MERGE_NOTE: &str = "AGENT AUTOBIOGRAPHY — these pages belong to an AI agent's OWN \
wiki, where the organising principle is one thread PER PERSON the agent works with. Two pages \
that differ by the person they are about (a diary of what was lived with A vs with B) must NEVER \
merge, however similar their prose — answer {\"merge\": false}. Only two pages about the same \
person, or about the agent itself, are candidates.";

/// The same rubric switch for the **contradiction** confirmer.
///
/// The satellites of an agent's fact are its relationship threads: what it
/// learned with one person does not stop being true because a fact about
/// another person changed. Without this, one revised preference can fell
/// neighbours from every other thread it happens to resemble.
const AGENT_CONTRADICTION_NOTE: &str = "AGENT AUTOBIOGRAPHY — these facts are an AI agent's OWN \
memory, one thread per person it works with. A fact that changed in ONE relationship never \
falsifies a similar-sounding fact from ANOTHER: only a satellite about the SAME subject with the \
SAME person can be superseded by this contradiction. What the agent is (its name, its voice, its \
role) changes only when the new fact states that change outright.";

fn revisor_prompt(
    tree: &WikiTree,
    new: &FactIndexRow,
    old: &FactIndexRow,
    agent_family: bool,
) -> Result<String> {
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
            (
                "subject_note",
                if agent_family { AGENT_DEDUP_NOTE } else { "" },
            ),
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

// ---------- Rail writer: the measured half of its nomination ----------
/// One page the walk keeps opening beside another it cannot reach from there
/// — a **missing link**, nominated deterministically.
///
/// **It names a direction, because that is what a reader walks.** A page pair
/// linked one way is a gap for the page at the far end and for nobody else, so
/// it is nominated once, for that page. A pair linked neither way is two gaps
/// and is nominated twice, once from each side: each of those pages has its own
/// reason to point, and a link puts no obligation on the page it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MissingRail {
    /// Plan slug of the page a reader would be standing on.
    from_slug: String,
    /// Plan slug of the page they could not walk to from there.
    to_slug: String,
    /// How many turns opened both.
    co_opens: usize,
}

/// Nominate the rails the corpus is missing: pages repeatedly opened
/// **together** by the walk, with no `[[wikilink]]` leading from one to the
/// other.
///
/// Deterministic, read-only, **no model call** — the behavioural nominator of
/// [`run_rail_writer`], and the strongest evidence there is: a reader opened
/// both and could not walk between them.
///
/// ⚠️ **It reads `recall_log`, so it is silent until there is traffic to
/// read.** On a memory nobody has talked to it returns nothing, which is
/// correct and is why the rail writer also nominates on **shape** — a page
/// carrying few links — instead of waiting for evidence that may be months
/// away.
///
/// Founder, 2026-08-04: *«è dal lavoro del REM che si conta la bontà della
/// memoria, perché i link che il navigatore segue alla fine li ha decisi il
/// REM»*.
///
/// **A link counts only if it is written on the page**, read with the same
/// extractor the funnel uses ([`crate::recall::extract_wikilinks`]).
///
/// The plan's `link_graph` is the obvious source — slug-keyed, and the graph
/// the compiler writes rails *from* — and it is the wrong one.
/// The compiler hands those links to the writing model as *recommended*, and
/// a model that does not weave one in leaves no link behind. The navigator
/// harvests rails from the **prose**, so a pair the plan calls linked can be
/// a pair the reader can never travel between: trusting the plan would hide
/// real gaps, silently and in the flattering direction.
///
/// `recall_log` stores workdir-relative page paths, so the plan's pages are
/// resolved to those paths through the tree to key the two together.
///
/// Reserved pages never nominate: the rules page is channel-only, so a rail
/// leading to it would point at a page nobody can open.
///
/// # Errors
///
/// Underlying `sqlx` errors from the recall-log read.
async fn detect_missing_rails(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &crate::planner::CompilationPlan,
    min_co_opens: usize,
    cap: usize,
) -> Result<Vec<MissingRail>> {
    if cap == 0 || min_co_opens == 0 {
        return Ok(Vec::new());
    }
    // workdir-relative source path -> plan slug, for the pages a walk can open,
    // plus the links each page actually carries.
    let mut by_path: BTreeMap<String, &str> = BTreeMap::new();
    let mut written: std::collections::BTreeSet<(&str, &str)> = std::collections::BTreeSet::new();
    let mut addr: BTreeMap<(String, String), &str> = BTreeMap::new();
    let mut bodies: Vec<(&str, String)> = Vec::new();
    for (slug, page) in &plan.pages {
        let rel = std::path::Path::new(&page.page_path);
        if crate::recall_nav::is_reserved_page_path(rel) {
            continue;
        }
        let Ok(wid) = crate::types::WikiId::parse(&page.wiki_id) else {
            continue;
        };
        let Ok(handle) = tree.locate(&wid) else {
            continue;
        };
        let abs = handle.abs_dir().join(rel);
        by_path.insert(
            crate::wiki::workdir_relative_source_path(tree.workdir(), &abs),
            slug.as_str(),
        );
        let stem = page
            .page_path
            .strip_suffix(".md")
            .unwrap_or(&page.page_path)
            .to_owned();
        addr.insert((page.wiki_id.clone(), stem), slug.as_str());
        if let Ok(body) = std::fs::read_to_string(&abs) {
            bodies.push((slug.as_str(), body));
        }
    }
    for (slug, body) in &bodies {
        for link in crate::recall::extract_wikilinks(body) {
            let Some(page) = link.page.as_deref() else {
                continue;
            };
            let key = (
                link.wiki_id.clone(),
                page.strip_suffix(".md").unwrap_or(page).to_owned(),
            );
            if let Some(target) = addr.get(&key) {
                written.insert((*slug, *target));
            }
        }
    }

    let sets = crate::recall_log::navigated_page_sets(pool, LINK_DETECTOR_SCAN_LIMIT).await?;
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for set in &sets {
        let mut slugs: Vec<&str> = set
            .iter()
            .filter_map(|p| by_path.get(p.as_str()).copied())
            .collect();
        slugs.sort_unstable();
        slugs.dedup();
        for (i, a) in slugs.iter().enumerate() {
            for b in &slugs[i + 1..] {
                *counts.entry((*a, *b)).or_default() += 1;
            }
        }
    }

    // Each way round is its own question, because each is its own walk: the
    // reader stands on one page and asks whether they can get to the other.
    // A pair joined one way leaves the far page just as stranded as a pair
    // joined neither way, and the side that already points has no gap at all.
    let mut out: Vec<MissingRail> = counts
        .into_iter()
        .filter(|&(_, n)| n >= min_co_opens)
        .flat_map(|((a, b), co_opens)| {
            [(a, b), (b, a)]
                .into_iter()
                .filter(|pair| !written.contains(pair))
                .map(|(from, to)| MissingRail {
                    from_slug: from.to_owned(),
                    to_slug: to.to_owned(),
                    co_opens,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // Strongest evidence first; slugs break ties so the list is stable across
    // runs and a diff between two cycles means something.
    out.sort_by(|x, y| {
        y.co_opens
            .cmp(&x.co_opens)
            .then_with(|| x.from_slug.cmp(&y.from_slug))
            .then_with(|| x.to_slug.cmp(&y.to_slug))
    });
    out.truncate(cap);
    Ok(out)
}

/// How many recent turns the rail detector scans. A month of production
/// traffic is ~800 turns, so this is a memory bound rather than a window:
/// the meaningful window is `recall_log`'s own retention.
const LINK_DETECTOR_SCAN_LIMIT: usize = 5_000;

// ---------- Auto-promote sub-job ----------
/// The page-mass floor for a page written in `style`, or `None` when mass is
/// not a reason to split that kind of page at all.
///
/// Founder's ruling, 2026-08-04, and the reasoning is about **how a page is
/// read**, not how long it is:
///
/// - **`lista`** — consulted, never read through: the shopping list, the films
///   seen. Its whole value is being complete in one place, so splitting it by
///   size breaks the one thing it is for and leaves two halves, neither of
///   which answers the question. **No floor** — mass is never a reason.
/// - **`prosa-tecnica`** — short bullets with brief descriptions, *scanned by
///   points*. Scanning tolerates more mass than following a narrative, so the
///   floor is higher.
/// - **`prosa`** (and anything unrecognised or absent) — the value is the
///   thread tying the facts together, and past a point there is no thread
///   left, only paragraphs side by side. Two pages with two threads beat one
///   without. The default floor.
///
/// Unrecognised styles fall to the prose floor deliberately: the palette is
/// closed and normalised at compile time, so an unknown value here is drift,
/// and drifting toward "may be split" is safer than toward "never".
/// Whether `path` (holding `mass` active facts) is over the floor its own
/// writing style sets — reading the style off the page's testata.
///
/// A page whose testata cannot be read at all falls to the prose floor: an
/// unreadable page is drift, and drifting toward "may be split" is safer than
/// toward "never split".
fn over_mass_floor(
    d: &crate::wiki::DiscoveredWiki,
    path: &str,
    mass: usize,
    policy: &RemPolicy,
) -> bool {
    mass_floor_for_style(page_style_of(d, path), policy).is_some_and(|floor| mass >= floor)
}

/// The writing style declared on a page's testata, or `None` when the page
/// cannot be read at all.
///
/// One reader for the floor and for the sentence the model is shown, so the
/// metre the engine measures by and the metre it names cannot drift apart.
fn page_style_of(d: &crate::wiki::DiscoveredWiki, path: &str) -> Option<crate::wiki::PageStyle> {
    wiki_relative_page(d, path)
        .and_then(|rel| crate::meta_annotate::read_page_card(&d.abs_dir.join(rel)).ok())
        .and_then(|card| card.style)
}

/// The sentence that tells the split model **which metre it is being measured
/// on**, from the page's own writing style.
///
/// The floor is not one number: a `prosa` page is a thread and loses it early,
/// a `prosa-tecnica` page is scanned by points and tolerates more, and a
/// `lista` is a set that is never split for size. Without this the model is
/// asked whether a page has "grown disproportionately" while the only scale it
/// has is the fact count — so it guesses at a rule the engine already knows,
/// and guesses the same way for a bullet list and a narrative.
fn shape_directive(style: Option<crate::wiki::PageStyle>, policy: &RemPolicy) -> String {
    use crate::wiki::PageStyle;
    match style {
        Some(PageStyle::Lista) => "This page is written as a `lista` — a set, \
             consulted rather than read through. **A list is never split for \
             size**: its whole value is being complete in one place, and two \
             halves are not two answers. Reply `{\"split\": false}` unless the \
             page genuinely mixes separable subjects."
            .to_owned(),
        Some(PageStyle::ProsaTecnica) => format!(
            "This page is written as `prosa-tecnica` — short points, SCANNED \
             rather than read through. Scanning tolerates mass, so the engine \
             only looks at a page of this kind from {floor} facts up, and you \
             are reading it because it passed that. Mass alone is therefore not \
             the question: the question is whether one subject in here reads as \
             separate from the others.",
            floor = policy.auto_promote_min_page_facts_technical
        ),
        Some(PageStyle::Prosa) | None => format!(
            "This page is written as `prosa` — a narrative thread tying its \
             facts together. Past a point there is no thread left, only \
             paragraphs side by side, so the engine looks at a page of this \
             kind from {floor} facts up and you are reading it because it \
             passed that. Two pages with two threads beat one page with none.",
            floor = policy.auto_promote_min_page_facts
        ),
    }
}

const fn mass_floor_for_style(
    style: Option<crate::wiki::PageStyle>,
    policy: &RemPolicy,
) -> Option<usize> {
    use crate::wiki::PageStyle;
    match style {
        // A list is a set: it is never split for size.
        Some(PageStyle::Lista) => None,
        Some(PageStyle::ProsaTecnica) => Some(policy.auto_promote_min_page_facts_technical),
        Some(PageStyle::Prosa) | None => Some(policy.auto_promote_min_page_facts),
    }
}

/// Per page over its **style's** floor ([`over_mass_floor`] — the only
/// deterministic gate, a resource pre-filter), show the **whole
/// page** to the `rem_promotions` LLM with each fact's 30-day recall
/// count and ask whether one sub-topic outgrew its siblings; on a
/// split verdict **apply the move directly** (act-first: born-applied
/// receipt, no pending proposal, no notice).
/// Hard-capped by `policy.auto_promote_cap`. A failed model call costs its
/// page and not the night — see [`note_llm_failure`].
///
/// **The pages tonight's reading will open are not asked here.**
/// `will_be_read` is the SELECTION ([`pages_the_round_wrote_onto`]), and the
/// reading asks each of those pages the same question while it has the page
/// open ([`SPLIT_QUESTION`]). This loop keeps what the reading never sees: the
/// page nobody wrote onto today, which grew heavy over weeks and is nominated
/// by mass alone. The regrouping above is not skipped for anybody — it reads
/// the whole shelf at once, and no single page's reading can answer it.
///
/// It is the SELECTION and not the readings that landed, and the difference is
/// a night: this pass runs before the reading does, so a page whose reading
/// then fails is not offered here until the next round. The rail writer, which
/// runs after, keys on the readings themselves.
///
/// No LLM → the sub-job short-circuits cleanly with
/// `disabled_reason = Some("no rem_promotions LLM wired")`.
#[allow(
    clippy::too_many_lines,
    reason = "filter + LLM call + dedup check + emit live as one orchestrator"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "one sub-job: the tree, who judges, under which policy, what the day did, and which pages another reading will open"
)]
async fn run_auto_promote(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: Option<&dyn LlmBackend>,
    cycle_id: &str,
    day: &day::DayPerimeter,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
    will_be_read: &BTreeSet<&str>,
) -> Result<AutoPromoteReport> {
    let mut report = AutoPromoteReport::default();
    let Some(llm) = llm else {
        report.disabled_reason = Some("no rem_promotions LLM wired".to_owned());
        return Ok(report);
    };
    // The whole tree up front: the grouping pass reads the memory as one
    // shelf, and it has to see every wiki that already exists to prefer
    // filing pages into one over founding a second home for the same subject.
    let all_wikis = tree.walk()?;
    // One run of failures for the whole sub-job, shared with the grouping
    // pass below: both call the same slot, so a backend that has stopped
    // answering one has stopped answering the other.
    let mut llm_failures = 0usize;

    // Page-group → wiki regrouping, ONCE for the whole memory and before the
    // per-page loop below. The two rungs of the forma fisica scale read
    // different signals and never contend: this one gathers whole pages that
    // are one argument, wherever they sit; the loop splits a single page that
    // has grown too heavy. A page this pass relocated is skipped below — each
    // wiki's `facts` snapshot is taken inside the loop and would still point
    // at the page's old home.
    let regrouped = run_page_grouping(
        &GroupingRun {
            pool,
            tree,
            llm,
            cycle_id,
            policy,
            all_wikis: &all_wikis,
            smart_wiki_index,
        },
        &mut llm_failures,
        &mut report,
    )
    .await?;
    if llm_failures >= LLM_FAILURE_ABORT {
        return Ok(report);
    }
    // The tree changed under us: pages moved between wikis, and a wiki may
    // have been born.
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
        if llm_failures >= LLM_FAILURE_ABORT {
            return Ok(report);
        }
        // Paragraph → page split, **per page** (rule 2 of the forma
        // scale): the LLM reads the whole page — every fact annotated
        // with its 30-day recall count — and decides whether one
        // sub-topic outgrew its siblings (mass) and/or is hot
        // (recall), naming the facts that move out. The page floor is
        // the only deterministic gate, a cheap resource pre-filter so
        // tiny pages never reach the LLM; everything semantic is the
        // LLM's call.
        // The floor depends on HOW the page is read, not just how big it is
        // (founder, 2026-08-04). See [`over_mass_floor`].
        let mut pages: Vec<&str> = page_mass
            .iter()
            // A channel page is never split (founder, 2026-08-18). Every
            // structural sweep fences it in its own way — dedup never pairs
            // across it, the completion sweep never takes it as evidence,
            // refile never nominates it as the fact to move — because a
            // channel's reader keys on the PATH: facts carried onto a page of
            // their own keep their words and stop being delivered, with no
            // error anywhere.
            .filter(|&(&path, _)| !wiki::is_channel_page(path))
            // The identity card is never split, whatever its mass (founder,
            // 2026-08-22). Recall serves it WHOLE into every turn: a split
            // moves facts onto a page the walk may never reach, so the turn
            // quietly stops being told them — and the card is the one page
            // whose whole job is that it cannot be missed. The mass floor
            // could not see this: the card is prose, so it hit the ordinary
            // 8-fact threshold like any topic page, and the promotions prompt
            // even offered `@profile.md` as its worked example of a page to
            // split.
            .filter(|&(&path, _)| !wiki::is_identity_card_page(path))
            .filter(|&(&path, &m)| over_mass_floor(d, path, m, policy))
            .map(|(&p, _)| p)
            .filter(|p| !regrouped.contains(*p))
            // A page tonight's reading opens answers this question inside
            // that reading, so asking it here would be the same page carried to
            // the same slot twice.
            .filter(|p| !will_be_read.contains(*p))
            .collect();
        // What the day added to first, then the heaviest, then the slug for
        // determinism. Sorting by slug alone hands the cap an alphabetical
        // list, and where a list is cut the order IS the selection: a page
        // that grew today would then wait for a night with room, behind pages
        // nothing has touched in months.
        let touched: HashSet<&str> = facts
            .iter()
            .filter(|f| day.touched_fact(f.fact_id.as_str()))
            .map(|f| f.source_path.as_str())
            .collect();
        pages.sort_by(|a, b| {
            touched
                .contains(b)
                .cmp(&touched.contains(a))
                .then_with(|| page_mass.get(b).cmp(&page_mass.get(a)))
                .then_with(|| a.cmp(b))
        });
        for source_path in pages {
            if report.applied.len() >= policy.auto_promote_cap {
                break;
            }
            let page_facts: Vec<&FactIndexRow> = facts
                .iter()
                .filter(|f| f.source_path == source_path)
                .collect();
            // The promote handler joins `source_page` onto the wiki's
            // abs_dir, so it must be wiki-relative (`cucina.md`), NOT the
            // fact's workdir-relative `source_path` (`wikis/<id>/cucina.md`):
            // the latter doubles the prefix and every apply misses on disk.
            // Compute it up front: it gates a cheap malformed-path skip AND
            // scopes the dedup below to receipts promoted FROM this page.
            let Some(source_page_rel) = wiki_relative_page(d, source_path) else {
                report.errors.push(format!(
                    "auto_promote: {source_path} is not under wiki {}",
                    d.meta.wiki_id.as_str(),
                ));
                continue;
            };
            // Coarse dedup: skip the page only if a genuine page-promotion
            // receipt (`paragraph_to_file`) already moved one of THESE
            // facts OUT OF THIS SAME page — the emergence pass
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
            // The metre this page was measured on, and the one the model is
            // about to be told about: read once, used for both.
            let style = page_style_of(d, source_path);
            let memo_prompt =
                paragraph_split_memo_prompt(tree, &source_page_rel, &page_facts, style, policy)?;
            let memo_key = rem_verdicts::key(llm.model_id(), &memo_prompt);
            if rem_verdicts::is_settled(pool, rem_verdicts::kind::PAGE_SPLIT, &memo_key).await? {
                continue;
            }
            report.candidates_examined += 1;

            let mass = page_facts.len();
            let prompt =
                paragraph_split_prompt(tree, &source_page_rel, &page_facts, style, policy)?;
            let resp = match llm
                .complete(
                    CompletionRequest::new(prompt)
                        .with_temperature(0.2)
                        .with_max_tokens(4_000),
                )
                .await
            {
                Ok(r) => {
                    llm_failures = 0;
                    r
                },
                Err(e) => {
                    let note = format!("auto_promote failed on page {source_page_rel}: {e}");
                    if note_llm_failure(&mut report.errors, &mut llm_failures, note) {
                        return Ok(report);
                    }
                    continue;
                },
            };
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
            // Third and last of the model-coined page names. A reserved
            // stem (`rules`, `projects`, `project_diary`, `projects_diary`,
            // `profile`, or anything starting with `@`) survives `slugify`
            // unchanged, and the split validator below checks only traversal
            // and "differs from the source" — so the reserved check has to be
            // here. A split has no fallback page to fall through to, so it is
            // simply skipped: the facts stay where they are and the next cycle
            // asks again.
            if crate::wiki::is_reserved_page_stem(&target_slug) {
                report.errors.push(format!(
                    "auto_promote: split target for {source_page_rel} named the reserved page \
                     {target_slug}.md — skipped",
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
            // A split moves a run of facts as one operation, so its receipt
            // cannot be written once per person the way a closure's can.
            // When the run belongs to more than one of them, the receipt is
            // addressed to nobody: it describes a change to the shape of the
            // memory that concerns all of them and is nobody's in
            // particular, and the alternative is telling whoever happens to
            // own the first fact how many of everybody else's moved with it.
            let recipient = {
                let mut people = moving
                    .iter()
                    .map(|f| proposals::recipient_from_fact(&f.subject_id, f.sender_id.as_ref()));
                let first = people.next().flatten();
                people.all(|p| p == first).then_some(first).flatten()
            };
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
                    report.split_targets.insert(target_slug.clone());
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
///   refiled / re-ACL'd fact veto its whole page. `paragraph_to_file` is
///   the one real page-promotion receipt to match. Its sibling
///   `pages_to_new_wiki` cannot be matched here and does not need to be:
///   its context carries a `pages` list and no `source_page`, so the
///   scope clause below would never fire on it.
/// - **Source scope.** A receipt records the page a fact was promoted
///   FROM. Matching `source_wiki_id`/`source_page` stops an old receipt
///   from vetoing a fact that has since migrated onto a *different* page
///   (a fact promoted off `appunti.md` that later landed on
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
           AND json_extract(context, '$.variant') = 'paragraph_to_file' \
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

/// Bundled default for the `rem-rails` system prompt.
/// See [`BUNDLED_REM_DEDUP_MD`] for the loader contract.
pub const BUNDLED_REM_RAILS_MD: &str = include_str!("../prompts/rem-rails.md");

/// How many links a page may carry before the rail writer stops adding to it.
///
/// **Not a cap on what a page may say** — the Cronista and the owner write
/// what the prose needs. It is the ceiling on what this pass will *push* a
/// page to, and past it a new rail has to take an old one's place (founder,
/// 2026-08-22: *«se su una pagina ci son già tanti collegamenti può decidere
/// di rimuoverne uno per far spazio ad un altro migliore»*).
///
/// Six, and where it comes from: the split pass only looks at a prose page
/// from eight facts up ([`RemPolicy::auto_promote_min_page_facts`]), so a page
/// holds a handful of facts, and the Cronista's own rule asks for *«a handful,
/// chosen; not a sweep»*. A page with more links than facts is not a page with good
/// links, it is a page of addresses.
///
/// Approved by the founder on 2026-08-23. It is still a number derived from
/// other numbers rather than from a corpus — there was no live one to look at —
/// so it is the one constant here that a first week of real pages is expected
/// to be allowed to move.
pub const PAGE_RAIL_BUDGET: usize = 6;

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
    style: Option<crate::wiki::PageStyle>,
    policy: &RemPolicy,
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
            ("shape", shape_directive(style, policy).as_str()),
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
    style: Option<crate::wiki::PageStyle>,
    policy: &RemPolicy,
) -> Result<String> {
    paragraph_split_prompt_inner(tree, page, page_facts, style, policy, false)
}

/// The canonical rendering hashed into the memo key: same template, same
/// facts, recall counters bucketed so day-to-day drift does not re-open
/// a page whose content has not moved.
fn paragraph_split_memo_prompt(
    tree: &WikiTree,
    page: &str,
    page_facts: &[&FactIndexRow],
    style: Option<crate::wiki::PageStyle>,
    policy: &RemPolicy,
) -> Result<String> {
    paragraph_split_prompt_inner(tree, page, page_facts, style, policy, true)
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
        .map(wiki::posix_path)
}

// ---------- Page-group → wiki regrouping ----------

/// What nominated a candidate group — said in words, because the receipt and
/// the dashboard show it and *«perché queste pagine»* is the first question a
/// reader has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Nominator {
    /// A topic word carried by the facts of every page in the group. The
    /// general case: the two words a fact carries exist to be counted, and a
    /// word many pages share is what "an argument" means here.
    Topic,
    /// The NAME of something that is not a principal — a relative, a pet, a
    /// car. One handle among several, never the privileged one.
    NamedThing,
    /// A single day. A day with enough happening in it is as much an argument
    /// as a subject is (founder, 2026-09-04).
    Day,
}

impl Nominator {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::NamedThing => "named thing",
            Self::Day => "day",
        }
    }
}

/// A group of pages the engine puts to the model, and what tied them.
#[derive(Debug, Clone)]
struct Candidate {
    /// The shared handle, verbatim: `salute`, `Bilbo`, `2026-06-30`.
    handle: String,
    nominator: Nominator,
    /// `<wiki_id>/<page.md>`, sorted, deduplicated.
    pages: Vec<String>,
}

/// Nominate the groups worth a question, from the whole memory, without
/// reading the whole memory.
///
/// **Three counts, no scan.** A candidate is any handle that ties at least
/// `floor` pages together, and the engine has three of them on columns it
/// already keeps: a fact's topic words, the name of what it is about, and the
/// day it holds from. Each is one `GROUP BY`, so the cost is the number of
/// ANSWERS, not the size of the memory — which is the whole reason this is not
/// "show the model every page and ask it to find groups". That version reads
/// fine on forty pages and is impossible on four thousand (founder,
/// 2026-09-04: *«su una memoria grande diventa insostenibile»*).
///
/// **No handle is privileged.** A wiki may emerge for anything that takes nine
/// pages — gardening, a car, a busy day — so the subject is one nominator
/// among three and not the shape the pass looks for.
///
/// Strongest first, so the night spends its cap on the clearest groups; the
/// caller drops pages an earlier group already claimed, which is what stops
/// two overlapping handles (`salute` and a person's name over nearly the same
/// pages) from minting two wikis for one argument.
async fn nominate_candidates(
    pool: &SqlitePool,
    all_wikis: &[wiki::DiscoveredWiki],
    smart_wiki_index: &SmartWikiIndex,
    floor: usize,
) -> Result<Vec<Candidate>> {
    let by_id: HashMap<&str, &wiki::DiscoveredWiki> = all_wikis
        .iter()
        .filter(|d| !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()))
        .map(|d| (d.meta.wiki_id.as_str(), d))
        .collect();

    // One read of the live rows, three groupings over it. The alternative —
    // three queries — reads the same rows three times for the same answer.
    let rows = fact_index::find_by_filters(pool, &fact_index::FactFilters::default()).await?;
    let mut buckets: HashMap<(Nominator, String), BTreeSet<String>> = HashMap::new();
    // Every page that holds a fact, by wiki — what a group is measured against
    // when the question is whether it already fills a wiki of its own.
    let mut wiki_pages: HashMap<String, BTreeSet<String>> = HashMap::new();
    for row in &rows {
        let Some(d) = by_id.get(row.wiki_id.as_str()) else {
            continue;
        };
        let Some(rel) = wiki_relative_page(d, &row.source_path) else {
            continue;
        };
        // The pages the engine names itself are never grouped: a card and a
        // rules page are found by their path, and moving one would silently
        // stop it being served.
        if wiki::is_identity_card_page(&row.source_path) || wiki::is_channel_page(&row.source_path)
        {
            continue;
        }
        let qualified = format!("{}/{rel}", row.wiki_id);
        wiki_pages
            .entry(row.wiki_id.clone())
            .or_default()
            .insert(qualified.clone());
        let mut add = |nominator, handle: String| {
            buckets
                .entry((nominator, handle))
                .or_default()
                .insert(qualified.clone());
        };
        for word in &row.topics {
            let word = word.trim();
            if !word.is_empty() {
                add(Nominator::Topic, word.to_owned());
            }
        }
        if let Some(name) = row.subject_external.as_deref().map(str::trim)
            && !name.is_empty()
        {
            add(Nominator::NamedThing, name.to_owned());
        }
        if let Some(day) = row.valid_from.as_deref().and_then(|v| v.get(..10))
            && day.len() == 10
        {
            add(Nominator::Day, day.to_owned());
        }
    }

    let mut out: Vec<Candidate> = buckets
        .into_iter()
        .filter(|(_, pages)| pages.len() >= floor)
        .filter(|(_, pages)| !already_fills_a_wiki(pages, &wiki_pages))
        .map(|((nominator, handle), pages)| Candidate {
            handle,
            nominator,
            pages: pages.into_iter().collect(),
        })
        .collect();
    // Widest first, ties by handle so two runs of the same memory ask the
    // same questions in the same order.
    out.sort_by(|a, b| {
        b.pages
            .len()
            .cmp(&a.pages.len())
            .then_with(|| a.handle.cmp(&b.handle))
    });
    Ok(out)
}

/// Is this group one wiki, whole? Then the argument already has its home.
///
/// The floor is a count, and a wiki born out of nine pages still has those
/// nine pages the next night: the same handle ties them again, and the same
/// question comes back for as long as the memory holds. There are only two
/// answers left to it and both are wrong — found a second wiki for the same
/// argument, or file the pages into the wiki they are already in. So the
/// group never reaches the model.
///
/// The test is deliberately narrow: **all** its pages in **one** wiki, and
/// that wiki holding no other page that carries a fact. A group that is nine
/// of a wiki's fifteen pages is a real question — those nine may well be a
/// subject of their own — and it is still asked.
fn already_fills_a_wiki(
    pages: &BTreeSet<String>,
    wiki_pages: &HashMap<String, BTreeSet<String>>,
) -> bool {
    let mut wikis = pages
        .iter()
        .filter_map(|p| p.split_once('/'))
        .map(|(wiki, _)| wiki);
    let Some(first) = wikis.next() else {
        return false;
    };
    if wikis.any(|w| w != first) {
        return false;
    }
    wiki_pages
        .get(first)
        .is_some_and(|all| all.len() == pages.len())
}

/// How many verbatim excerpts of a page ride in the inventory the
/// cartographer reads. Two is enough to tell `bagnetto_neonata.md` from
/// `bucato_neonata.md` without shipping the whole corpus in the prompt.
const GROUPING_SNIPPETS_PER_PAGE: usize = 2;

/// Character budget per excerpt.
const GROUPING_SNIPPET_CHARS: usize = 110;

/// The *page-group → wiki* regrouping pass — the page→folder rung of the
/// forma fisica scale, read across the WHOLE memory.
///
/// **The engine nominates, the model judges.** [`nominate_candidates`] counts
/// three handles a page can share — a topic word, the name of what a fact is
/// about, a day — and hands over every group of at least
/// `auto_promote_group_min_pages` pages. Each candidate is then one small
/// question: *these pages share this handle, are they one subject area, and
/// what is the wiki called?*
///
/// It is deliberately **not** "show the model every page and let it find the
/// groups". That reads fine on forty pages and is impossible on four thousand
/// (founder, 2026-09-04: *«su una memoria grande diventa insostenibile»*), and
/// it also asks the model to do the counting, which is the one part a
/// `GROUP BY` does better and explains for free — the receipt can say WHY
/// these pages were put together.
///
/// **Nothing is privileged.** A wiki may emerge for anything that takes nine
/// pages: gardening, a car, a cat, a day with a lot in it, a relative the
/// household looks after. The subject is one nominator of three.
///
/// The pages a group takes are dropped from every later candidate of the same
/// night, so two handles lying over the same pages — a topic and a name, say —
/// mint one wiki and not two. A candidate that is one wiki whole is never put
/// at all: see [`already_fills_a_wiki`].
/// The read-only half of a grouping run, gathered once by the caller.
///
/// The pass puts one question per candidate to the model and applies at most
/// `auto_promote_cap` answers; every one of them reads the same memory, and
/// the tree walk behind `all_wikis` is expensive enough to be worth doing once.
struct GroupingRun<'a> {
    pool: &'a SqlitePool,
    tree: &'a WikiTree,
    llm: &'a dyn LlmBackend,
    cycle_id: &'a str,
    policy: &'a RemPolicy,
    all_wikis: &'a [wiki::DiscoveredWiki],
    smart_wiki_index: &'a SmartWikiIndex,
}

/// What one candidate left behind.
enum CandidateOutcome {
    /// Nothing moved — the model saw no subject area, or its answer was
    /// unusable. The next candidate is asked all the same.
    Nothing,
    /// The group was applied: these pages, qualified as `<wiki>/<page.md>`,
    /// are now somewhere else.
    Applied(Vec<String>),
    /// The slot stopped answering. Nothing more is asked tonight.
    Abort,
}

async fn run_page_grouping(
    run: &GroupingRun<'_>,
    llm_failures: &mut usize,
    report: &mut AutoPromoteReport,
) -> Result<HashSet<String>> {
    let mut moved: HashSet<String> = HashSet::new();
    if report.applied.len() >= run.policy.auto_promote_cap {
        return Ok(moved);
    }
    let candidates = nominate_candidates(
        run.pool,
        run.all_wikis,
        run.smart_wiki_index,
        run.policy.auto_promote_group_min_pages,
    )
    .await?;
    if candidates.is_empty() {
        tracing::info!(
            floor = run.policy.auto_promote_group_min_pages,
            "rem grouping: nothing ties enough pages together to be worth a wiki"
        );
        return Ok(moved);
    }

    // Where each page lives, so a group can be applied without re-walking.
    let by_id: HashMap<&str, &wiki::DiscoveredWiki> = run
        .all_wikis
        .iter()
        .map(|d| (d.meta.wiki_id.as_str(), d))
        .collect();
    let existing = grouping_existing_wikis_all(run.all_wikis, run.smart_wiki_index);
    let mut claimed: HashSet<String> = HashSet::new();

    for candidate in candidates {
        if report.applied.len() >= run.policy.auto_promote_cap {
            break;
        }
        let pages: Vec<String> = candidate
            .pages
            .iter()
            .filter(|p| !claimed.contains(*p))
            .cloned()
            .collect();
        if pages.len() < run.policy.auto_promote_group_min_pages {
            continue;
        }
        let outcome = judge_one_candidate(
            run,
            &candidate,
            &pages,
            &by_id,
            &existing,
            llm_failures,
            report,
        )
        .await?;
        match outcome {
            CandidateOutcome::Nothing => {},
            CandidateOutcome::Abort => return Ok(moved),
            CandidateOutcome::Applied(kept) => {
                for qualified in kept {
                    if let Some(source_path) = source_path_of(&by_id, &qualified) {
                        moved.insert(source_path);
                    }
                    claimed.insert(qualified);
                }
            },
        }
    }

    Ok(moved)
}

/// Put one candidate to the model and act on the answer.
async fn judge_one_candidate(
    run: &GroupingRun<'_>,
    candidate: &Candidate,
    pages: &[String],
    by_id: &HashMap<&str, &wiki::DiscoveredWiki>,
    existing: &str,
    llm_failures: &mut usize,
    report: &mut AutoPromoteReport,
) -> Result<CandidateOutcome> {
    let inventory = candidate_inventory(run.pool, by_id, pages).await?;
    let prompt = candidate_grouping_prompt(
        run.tree,
        candidate,
        pages.len(),
        existing,
        &inventory,
        run.pool,
    )
    .await?;
    let memo_key = rem_verdicts::key(run.llm.model_id(), &prompt);
    if rem_verdicts::is_settled(run.pool, rem_verdicts::kind::PAGE_GROUPING, &memo_key).await? {
        tracing::info!(
            handle = candidate.handle.as_str(),
            nominator = candidate.nominator.as_str(),
            pages = pages.len(),
            "rem grouping: this group was already judged — not asked again"
        );
        return Ok(CandidateOutcome::Nothing);
    }
    report.grouping_wikis_examined += 1;

    let resp = match run
        .llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.2)
                .with_max_tokens(1_200),
        )
        .await
    {
        Ok(r) => {
            *llm_failures = 0;
            r
        },
        Err(e) => {
            note_llm_failure(
                &mut report.errors,
                llm_failures,
                format!(
                    "page grouping failed on {handle}: {e}",
                    handle = candidate.handle
                ),
            );
            return Ok(CandidateOutcome::Abort);
        },
    };
    let Some(groups) = parse_page_groups(&resp.text) else {
        report.errors.push(format!(
            "page grouping llm returned unparseable verdict for {handle}",
            handle = candidate.handle,
        ));
        return Ok(CandidateOutcome::Nothing);
    };
    // One question, one answer: the pass shows a candidate and asks whether it
    // is a subject area, so a reply naming several groups is answering a
    // question nobody put. The first is taken and the rest ignored rather than
    // refused — the extra ones name the same pages.
    let Some(group) = groups.into_iter().next() else {
        rem_verdicts::record_negative(
            run.pool,
            rem_verdicts::kind::PAGE_GROUPING,
            &memo_key,
            &candidate.handle,
        )
        .await?;
        return Ok(CandidateOutcome::Nothing);
    };
    // The model may keep a subset: pages it judges to be off the subject stay
    // where they are. What it may NOT do is name a page nobody offered it.
    let kept: Vec<String> = if group.pages.is_empty() {
        pages.to_vec()
    } else {
        let offered: HashSet<&str> = pages.iter().map(String::as_str).collect();
        let unknown: Vec<&String> = group
            .pages
            .iter()
            .filter(|p| !offered.contains(p.as_str()))
            .collect();
        if !unknown.is_empty() {
            report.errors.push(format!(
                "page grouping named pages nobody offered for {handle}: {unknown:?}",
                handle = candidate.handle,
            ));
            return Ok(CandidateOutcome::Nothing);
        }
        group.pages.clone()
    };

    let Some(kept) = pages_the_action_moves(&group.action, kept, run.policy) else {
        return Ok(CandidateOutcome::Nothing);
    };

    apply_one_group(
        run,
        candidate,
        &group.action,
        kept,
        pages.len(),
        by_id,
        report,
    )
    .await
}

/// Which of the named pages the action actually moves, or `None` when it moves
/// nothing.
///
/// The two actions read the count differently. **Birth** is what the floor
/// governs: a group founding a wiki has to be worth one, and a subset that
/// falls under the floor is not. **Joining** a wiki that exists has nothing to
/// justify — the home is there, and one stray page belongs inside as much as
/// nine do — but a group nominated across the whole memory routinely names
/// pages that are already in the target. Those need no move, and the apply
/// refuses a page that is already home, taking the whole group with it.
fn pages_the_action_moves(
    action: &GroupAction,
    kept: Vec<String>,
    policy: &RemPolicy,
) -> Option<Vec<String>> {
    match action {
        GroupAction::Create { .. } => {
            (kept.len() >= policy.auto_promote_group_min_pages).then_some(kept)
        },
        GroupAction::Move { target } => {
            let elsewhere: Vec<String> = kept
                .into_iter()
                .filter(|q| q.split_once('/').is_none_or(|(wiki, _)| wiki != target))
                .collect();
            (!elsewhere.is_empty()).then_some(elsewhere)
        },
    }
}

/// Carry out what the model decided for one group, under a WAL op.
async fn apply_one_group(
    run: &GroupingRun<'_>,
    candidate: &Candidate,
    action: &GroupAction,
    kept: Vec<String>,
    offered: usize,
    by_id: &HashMap<&str, &wiki::DiscoveredWiki>,
    report: &mut AutoPromoteReport,
) -> Result<CandidateOutcome> {
    let recipient = recipient_of_first_page(run.pool, &kept).await;
    // Two numbers, because they are two different facts and the receipt is
    // read by a person: how many pages share the handle, and how many this
    // move carries. A group of nine whose eight are already home moves one,
    // and a receipt saying "1 pages share the topic" would be false.
    let hints = promote::PageGroupHints {
        group_pages: Some(kept.len()),
        source_wiki_pages: None,
        reason: Some(if kept.len() == offered {
            format!(
                "rem grouping: {offered} pages share the {kind} `{handle}`",
                kind = candidate.nominator.as_str(),
                handle = candidate.handle,
            )
        } else {
            format!(
                "rem grouping: {offered} pages share the {kind} `{handle}`, {moved} of them moved",
                kind = candidate.nominator.as_str(),
                handle = candidate.handle,
                moved = kept.len(),
            )
        }),
    };

    let (op_id, res, variant) = match action {
        GroupAction::Create {
            slug,
            title,
            style,
            description,
        } => {
            let op_id =
                wal::begin_rem_op(run.pool, run.cycle_id, "page_grouping_create", None, None)
                    .await?;
            let res = promote::apply_pages_to_new_wiki_direct(
                run.pool,
                run.tree,
                &kept,
                slug,
                title.as_deref(),
                style.map(crate::wiki::PageStyle::as_str),
                description.as_deref(),
                &hints,
                recipient,
            )
            .await;
            (op_id, res, "pages_to_new_wiki")
        },
        GroupAction::Move { target } => {
            if !by_id.contains_key(target.as_str()) {
                report
                    .errors
                    .push(format!("page grouping named {target}, which is not a wiki"));
                return Ok(CandidateOutcome::Nothing);
            }
            let op_id = wal::begin_rem_op(
                run.pool,
                run.cycle_id,
                "page_grouping_move",
                Some(target),
                None,
            )
            .await?;
            let res = promote::apply_pages_into_wiki_direct(
                run.pool, run.tree, target, &kept, &hints, recipient,
            )
            .await;
            (op_id, res, "pages_into_wiki")
        },
    };

    match res {
        Ok(receipt) => {
            wal::complete_rem_op(run.pool, op_id).await?;
            report.applied.push(receipt.proposal_id.clone());
            report.grouping_groups_applied += 1;
            Ok(CandidateOutcome::Applied(kept))
        },
        Err(e) => {
            wal::fail_rem_op(run.pool, op_id, &format!("{e}")).await?;
            report.errors.push(format!("apply {variant} failed: {e}"));
            Ok(CandidateOutcome::Nothing)
        },
    }
}

/// Every standard wiki a group could be filed into, with what each holds.
///
/// A group is judged against the whole memory, so every wiki that is not smart
/// is a possible home — a shelf is not a property, and none of them is off
/// limits.
fn grouping_existing_wikis_all(
    all_wikis: &[wiki::DiscoveredWiki],
    smart_wiki_index: &SmartWikiIndex,
) -> String {
    use std::fmt::Write as _;
    let standard: Vec<&wiki::DiscoveredWiki> = all_wikis
        .iter()
        .filter(|d| !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()))
        .collect();
    if standard.is_empty() {
        return "(none)\n".to_owned();
    }
    let mut out = String::new();
    for c in standard {
        let summary = c
            .meta
            .extra
            .get(serde_yaml::Value::from("summary"))
            .and_then(serde_yaml::Value::as_str)
            .unwrap_or("");
        // Topic pages: a foundation page is not one, and counting it would
        // put every wiki one ahead of what it actually holds on the subject.
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
            "- {id} \"{title}\" [{pages} pages]{sep}{summary}",
            id = c.meta.wiki_id,
            title = c.meta.title,
            sep = if summary.is_empty() { "" } else { " — " },
        );
    }
    out
}

/// `wikis/<…>/<page.md>` for a `<wiki_id>/<page.md>`, or `None` when the wiki
/// is gone from the tree.
fn source_path_of(by_id: &HashMap<&str, &wiki::DiscoveredWiki>, qualified: &str) -> Option<String> {
    let (wiki, page) = qualified.split_once('/')?;
    let d = by_id.get(wiki)?;
    Some(d.source_path(std::path::Path::new(page)))
}

/// The inventory of ONE candidate: its pages, each with its fact count and a
/// couple of verbatim excerpts.
///
/// Same shape and same reasoning as the whole-wiki ancestor — the stored page
/// description drifts, so the file name plus two real sentences is what the
/// model is given — but bounded by the candidate instead of by a wiki, which
/// is what keeps the prompt the same size on a memory of any size.
async fn candidate_inventory(
    pool: &SqlitePool,
    by_id: &HashMap<&str, &wiki::DiscoveredWiki>,
    pages: &[String],
) -> Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    for qualified in pages {
        let Some(source_path) = source_path_of(by_id, qualified) else {
            continue;
        };
        let facts = fact_index::find_active_by_source_path(pool, &source_path).await?;
        let _ = write!(out, "- {qualified} [{n}]", n = facts.len());
        for (shown, f) in facts.iter().take(GROUPING_SNIPPETS_PER_PAGE).enumerate() {
            let sep = if shown == 0 { " — " } else { " / " };
            let snippet = truncate_chars(f.text.trim(), GROUPING_SNIPPET_CHARS);
            let _ = write!(out, "{sep}\"{snippet}\"");
        }
        out.push('\n');
    }
    Ok(out)
}

/// Who the receipt is addressed to: the first page's first fact answers, the
/// way the per-wiki ancestor did.
async fn recipient_of_first_page(pool: &SqlitePool, pages: &[String]) -> Option<String> {
    let first = pages.first()?;
    let (_, page) = first.split_once('/')?;
    let rows = fact_index::find_by_filters(pool, &fact_index::FactFilters::default())
        .await
        .ok()?;
    rows.iter()
        .find(|r| r.source_path.ends_with(page))
        .and_then(|f| proposals::recipient_from_fact(&f.subject_id, f.sender_id.as_ref()))
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

/// Render the question put about ONE nominated candidate.
///
/// The placeholders carry what nominated the group (`{handle}`,
/// `{nominator}`) so the model judges the tie the engine actually found,
/// rather than hunting for one of its own. The language is the memory's
/// default: a wiki that does not exist yet has none to declare, and the
/// pages come from several wikis that may not agree.
async fn candidate_grouping_prompt(
    tree: &WikiTree,
    candidate: &Candidate,
    pages: usize,
    existing: &str,
    inventory: &str,
    pool: &SqlitePool,
) -> Result<String> {
    let pages_s = pages.to_string();
    let locale = crate::locale::memory_wide_locale(pool).await;
    let language_directive = crate::locale::render_memory_language_directive(locale.as_deref());
    prompts::render(
        "rem-page-grouping",
        tree.workdir(),
        BUNDLED_REM_PAGE_GROUPING_MD,
        &[
            ("locale", language_directive.as_str()),
            ("handle", candidate.handle.as_str()),
            ("nominator", candidate.nominator.as_str()),
            ("pages", pages_s.as_str()),
            ("existing", existing),
            ("inventory", inventory),
        ],
    )
    .map_err(RemError::from)
}

/// What the cartographer decided to do with one group of pages.
#[derive(Debug, Clone)]
enum GroupAction {
    /// Found a new wiki for them at the top level (subject to the page
    /// floor).
    Create {
        slug: String,
        title: Option<String>,
        /// Dominant style **default** for the newborn wiki's `_meta`, or
        /// `None` when genuinely mixed. A hint, not a gate.
        style: Option<crate::wiki::PageStyle>,
        /// Free-text scope for the newborn wiki's `_meta`.
        description: Option<String>,
    },
    /// File them into a wiki that already exists.
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
                    style: crate::wiki::PageStyle::parse_lenient(str_field("style").as_deref()),
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

/// Bundled default for the structural review prompt; workdir override:
/// `<workdir>/prompts/rem-structure.md`.
pub const BUNDLED_REM_STRUCTURE_MD: &str = include_str!("../prompts/rem-structure.md");

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
/// **concept leaves of the same wiki** (`consolidating` holds every wiki a
/// pair may come from; a wiki absent from it, smart or vanished, never
/// pairs), deduped, capped at `cap`
/// (the resource bound on confirmation calls). Returns
/// `(slug_a, slug_b, signal)`.
fn merge_candidates(
    plan: &CompilationPlan,
    duplicate_prose: &[(String, String, f32)],
    consolidating: &BTreeSet<String>,
    day: &day::DayPerimeter,
    split_targets: &std::collections::BTreeSet<String>,
) -> Vec<(String, String, String)> {
    fn eligible<'p>(plan: &'p CompilationPlan, slug: &str) -> Option<&'p PagePlan> {
        plan.pages
            .get(slug)
            .filter(|p| !p.is_identity_card() && !p.primary_facts.is_empty())
    }
    let same_scope = |a: &str, b: &str| a == b && consolidating.contains(a);
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut consider = |a: &str, b: &str, signal: String| {
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        // A page this cycle's split just coined is the same subject as the
        // page it came out of — the confirmer would be right to say so, and
        // wrong to act on it. See `AutoPromoteReport::split_targets`.
        if split_targets.contains(x) || split_targets.contains(y) {
            return;
        }
        let (Some(pa), Some(pb)) = (eligible(plan, x), eligible(plan, y)) else {
            return;
        };
        if !same_scope(&pa.wiki_id, &pb.wiki_id) {
            return;
        }
        if seen.insert((x.to_owned(), y.to_owned())) {
            out.push((x.to_owned(), y.to_owned(), signal));
        }
    };
    // Strongest signal first, and the whole list comes back: the caller
    // spends its budget on pairs that actually reach a judgement.
    //
    // The prose pairs lead, ranked by how much text they share — that is a
    // measured overlap, where kinship is only a name resembling a name. The
    // sort is load-bearing: the plan's `BTreeMap` hands them over by slug, and
    // a budget cut off the front of an alphabetical list judges pairs for
    // their initial.
    let mut prose: Vec<&(String, String, f32)> = duplicate_prose.iter().collect();
    prose.sort_by(|a, b| b.2.total_cmp(&a.2));
    for (a, b, score) in prose {
        consider(a, b, format!("duplicate prose, jaccard {score:.2}"));
    }
    // Heaviest pages first among the kin pairs: a pair carrying a hundred
    // facts between them is a bigger duplication than a pair carrying three,
    // and mass is the signal available without a second measurement.
    let mut leaves: Vec<&PagePlan> = plan
        .pages
        .values()
        .filter(|p| !p.is_identity_card() && !p.primary_facts.is_empty())
        .collect();
    leaves.sort_by_key(|p| std::cmp::Reverse(p.primary_facts.len()));
    for (i, p) in leaves.iter().enumerate() {
        for q in leaves.iter().skip(i + 1) {
            if same_scope(&p.wiki_id, &q.wiki_id) && slug_kinship(&p.slug, &q.slug) {
                consider(&p.slug, &q.slug, "page-name kinship".to_owned());
            }
        }
    }
    // A pair the day touched leads, whatever put it on the list. The caller
    // spends a fixed budget of judgements: two pages that both stopped moving
    // months ago will still be there tomorrow, and a page opened this morning
    // for a single claim is the one that most needs somewhere better to be.
    // A stable sort, so the signal ranking above survives inside each band.
    let touched = |slug: &str| {
        day.touched_page(slug)
            || plan.pages.get(slug).is_some_and(|p| {
                p.primary_facts
                    .iter()
                    .any(|f| day.touched_fact(f.fact_id.as_str()))
            })
    };
    out.sort_by_key(|(a, b, _)| std::cmp::Reverse(touched(a) || touched(b)));
    out
}

/// Whether a page-merge receipt already covers this pair (either
/// orientation). A receipt means the pair was judged once; it is not
/// re-judged.
///
/// **Matched as whole JSON values, with the wiki, in one orientation or the
/// other** — never an unanchored `LIKE '%<page>%'` over the context blob. A
/// substring match would let any page name that contains another inherit its
/// verdict: once `lista_spesa.md` + `dispensa.md` had been judged anywhere on
/// the machine, `spesa.md` + `dispensa.md` would count as judged, in every
/// wiki, forever. A veto is an operator's decision about two specific pages;
/// spreading it by substring makes it a decision about names nobody chose.
async fn merge_already_judged(
    pool: &SqlitePool,
    wiki_a: &str,
    page_a: &str,
    wiki_b: &str,
    page_b: &str,
) -> Result<bool> {
    // The context is a compact JSON object, so a `"key":"value"` fragment
    // matches the whole value and nothing longer. One LIKE per key keeps this
    // independent of the object's key order.
    let field = |key: &str, value: &str| format!("%\"{key}\":\"{value}\"%");
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM structure_proposals
          WHERE kind = 'wiki_promote'
            AND context LIKE '%\"variant\":\"page_merge\"%'
            AND (
                 (context LIKE ? AND context LIKE ? AND context LIKE ? AND context LIKE ?)
              OR (context LIKE ? AND context LIKE ? AND context LIKE ? AND context LIKE ?)
            )",
    )
    // Orientation 1 — a was the husk, b the survivor.
    .bind(field("source_wiki_id", wiki_a))
    .bind(field("source_page", page_a))
    .bind(field("target_wiki_id", wiki_b))
    .bind(field("recommended_target_page", page_b))
    // Orientation 2 — the other way round.
    .bind(field("source_wiki_id", wiki_b))
    .bind(field("source_page", page_b))
    .bind(field("target_wiki_id", wiki_a))
    .bind(field("recommended_target_page", page_a))
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// One page's block for the merge prompt: identity + numbered claims.
/// The `wiki:` line matters on a family-scope pair — the judge sees
/// which of the two wikis each page lives in.
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
        p.style.map_or("—", |s| s.as_str()),
    )
}

fn merge_prompt(
    tree: &WikiTree,
    a: &PagePlan,
    b: &PagePlan,
    signal: &str,
    agent_family: bool,
) -> Result<String> {
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
            (
                "subject_note",
                if agent_family { AGENT_MERGE_NOTE } else { "" },
            ),
        ],
    )?)
}

/// How many pages the structural review may be shown at once.
///
/// The judge is asked to weigh a whole forest, and a forest that does not fit
/// in one answer is not one it can weigh. Above this the inventory keeps the
/// pages whose facts are mostly about somebody other than their wiki's own
/// principal — the cheap signal for the mistake being looked for — and says
/// how many it left out, in the report AND in the prompt: a judge that thinks
/// it saw everything draws conclusions it has not earned.
const STRUCTURE_INVENTORY_PAGES: usize = 120;

/// Build the forest as the structural review sees it.
///
/// One line per page: where it sits, what its card says it holds, how many
/// facts it carries, and — the part that matters — the principals those facts
/// are actually ABOUT. A page's name and its wiki are what the engine decided;
/// the subjects are what the page IS.
///
/// A **topic wiki** — named for its subject, standing for nobody, which is
/// what the nightly grouping raises — answers to no principal, so its entry
/// carries `None`. Its pages are in the inventory like any other: they are the
/// ones the grouping just placed, and a review that cannot see them cannot
/// weigh the forest it is being asked about.
fn forest_inventory(tree: &WikiTree, plan: &crate::planner::CompilationPlan) -> Vec<ForestPage> {
    let principals: BTreeMap<String, Option<String>> = tree
        .walk()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| !d.meta.smart)
        .filter_map(|d| {
            let p = tree.resolve_scope_principal(&d.meta).ok()?;
            Some((d.meta.wiki_id.as_str().to_owned(), p.map(|p| p.to_string())))
        })
        .collect();
    let mut out = Vec::new();
    for page in plan.pages.values() {
        // A card is its wiki's by construction and a reserved page is found by
        // its path; neither is ever in the wrong wiki.
        if page.is_identity_card()
            || crate::wiki::names_reserved_page(std::path::Path::new(&page.page_path))
        {
            continue;
        }
        if page.primary_facts.is_empty() {
            continue;
        }
        let Some(own) = principals.get(&page.wiki_id) else {
            continue; // smart, or vanished between the plan and the walk
        };
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for f in &page.primary_facts {
            *tally.entry(f.subject.to_string()).or_default() += 1;
        }
        let mut subjects: Vec<(String, usize)> = tally.into_iter().collect();
        subjects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        // "Off principal" is a comparison against the wiki's own principal, so
        // no page of a topic wiki is off one: which facts belong there is a
        // question about the wiki's subject, not about who owns it.
        let off_principal = own
            .as_ref()
            .is_some_and(|own| subjects.first().is_some_and(|(s, _)| s != own));
        out.push(ForestPage {
            address: format!("{}/{}", page.wiki_id, page.page_path),
            wiki_id: page.wiki_id.clone(),
            page: page.page_path.clone(),
            card: page.description.clone(),
            facts: page.primary_facts.len(),
            subjects,
            off_principal,
        });
    }
    // Under the cap the off-principal pages go first: they are where the
    // mistake lives, and what falls off the end is what nothing suggests is
    // wrong. Stable within each half so the inventory does not reshuffle
    // between cycles for no reason.
    out.sort_by(|a, b| {
        b.off_principal
            .cmp(&a.off_principal)
            .then_with(|| a.address.cmp(&b.address))
    });
    out
}

/// Render the inventory the judge reads.
fn render_forest(tree: &WikiTree, pages: &[ForestPage]) -> String {
    use std::fmt::Write as _;
    let mut by_wiki: BTreeMap<&str, Vec<&ForestPage>> = BTreeMap::new();
    for p in pages {
        by_wiki.entry(p.wiki_id.as_str()).or_default().push(p);
    }
    let principals: BTreeMap<String, Option<String>> = tree
        .walk()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| !d.meta.smart)
        .filter_map(|d| {
            let p = tree.resolve_scope_principal(&d.meta).ok()?;
            Some((d.meta.wiki_id.as_str().to_owned(), p.map(|p| p.to_string())))
        })
        .collect();
    let mut out = String::new();
    for (wiki, ps) in by_wiki {
        // The judge is told whose a wiki is because rule 1 of its prompt is a
        // comparison against that. A topic wiki is nobody's, and saying so is
        // what stops the rule from firing on every page the grouping just
        // placed.
        let whose = match principals.get(wiki) {
            Some(Some(p)) => p.clone(),
            Some(None) => "nobody — it is named for its subject".to_owned(),
            None => "?".to_owned(),
        };
        let _ = writeln!(out, "\nwiki {wiki} — belongs to {whose}");
        for p in ps {
            let subjects = p
                .subjects
                .iter()
                .map(|(s, n)| format!("{s} ({n})"))
                .collect::<Vec<_>>()
                .join(", ");
            let card = if p.card.trim().is_empty() {
                "(no card)"
            } else {
                p.card.trim()
            };
            let _ = writeln!(
                out,
                "  {}/{} · {} facts · subjects: {}\n      {}",
                p.wiki_id, p.page, p.facts, subjects, card
            );
        }
    }
    out
}

/// The structural review — the only pass that looks at the whole forest.
///
/// Every other sub-job is scoped to one wiki, one page or one fact, and each
/// repairs what it can see from there. None of them can see the mistake this
/// one looks for: a page in the **wrong wiki**. That mistake is made once, in
/// a second, when the page is born — a new page joins the wiki of whichever of
/// its facts the classifier listed first — and it has to be repaired at the
/// grain it was made. The refile sweep moves facts, one judgment each, which
/// asks the same question forty times for a forty-fact page and gets forty
/// independent answers.
///
/// Structural signals **nominate** (the inventory's order under the cap: pages
/// whose facts are mostly about somebody other than their wiki's principal);
/// the strong model **decides**, and it is shown the forest rather than a
/// short-list so it can refuse. Confirmed moves land act-first via
/// [`promote::apply_pages_rehome_direct`] with a receipt carrying the judge's
/// own sentence — the move stands, so somebody has to be able to read why.
async fn run_structure_review(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    policy: &RemPolicy,
) -> Result<StructureReviewReport> {
    let mut report = StructureReviewReport::default();
    if policy.structure_review_cap == 0 {
        return Ok(report);
    }
    let plan = match crate::planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(report),
        Err(e) => {
            report
                .errors
                .push(format!("structure: plan load failed: {e}"));
            return Ok(report);
        },
    };
    let all = forest_inventory(tree, &plan);
    if all.len() < 2 {
        return Ok(report); // one page cannot be in the wrong wiki
    }
    let shown = all.len().min(STRUCTURE_INVENTORY_PAGES);
    report.pages_shown = shown;
    report.pages_dropped = all.len() - shown;
    let dropped_note = if report.pages_dropped > 0 {
        format!(
            "NOTE: you are shown {shown} pages of {}. The {} left out are the ones nothing \
             suggests are misplaced. Do not conclude anything about the memory as a whole.\n",
            all.len(),
            report.pages_dropped
        )
    } else {
        String::new()
    };
    // The applier below takes the first `structure_review_cap` moves, so the
    // model is told that number rather than a copy of it written into the
    // prompt body: a second number is a number that drifts.
    let cap = policy.structure_review_cap.to_string();
    let prompt = prompts::render(
        "rem-structure",
        tree.workdir(),
        BUNDLED_REM_STRUCTURE_MD,
        &[
            ("forest", render_forest(tree, &all[..shown]).as_str()),
            ("dropped", dropped_note.as_str()),
            ("cap", cap.as_str()),
        ],
    )?;
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::STRUCTURE, &memo_key).await? {
        return Ok(report);
    }
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(900),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rem structure: judge unavailable — skipped");
            return Ok(report);
        },
    };
    let Some(raw) = first_json_object(&resp.text) else {
        tracing::warn!("rem structure: unparseable judge answer — skipped");
        return Ok(report);
    };
    let decision: StructureDecision = match serde_json::from_value(raw) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "rem structure: bad judge JSON shape — skipped");
            return Ok(report);
        },
    };
    report.moves_named = decision.moves.len();
    if decision.moves.is_empty() {
        // The house pattern: a clean "nothing to move" is a memo, so the same
        // forest is not re-judged tomorrow night. An unparseable answer above
        // is NOT — that is a failure, and a failure must be retried.
        rem_verdicts::record_negative(pool, rem_verdicts::kind::STRUCTURE, &memo_key, "forest")
            .await?;
        return Ok(report);
    }
    apply_structure_moves(
        pool,
        tree,
        &all[..shown],
        &decision.moves,
        policy.structure_review_cap,
        &mut report,
    )
    .await;
    Ok(report)
}

/// Execute the moves the judge named, up to the cap.
///
/// Anti-hallucination, exactly like every other act-first sub-job: only an
/// address the judge was SHOWN can move, so an invented page is an error on
/// the report rather than a guess acted on. A move naming the page's own wiki
/// is not an error at all — it is the judge saying nothing — and it stays
/// silent.
async fn apply_structure_moves(
    pool: &SqlitePool,
    tree: &WikiTree,
    shown: &[ForestPage],
    moves: &[StructureMove],
    cap: usize,
    report: &mut StructureReviewReport,
) {
    let by_address: BTreeMap<&str, &ForestPage> =
        shown.iter().map(|p| (p.address.as_str(), p)).collect();
    for mv in moves.iter().take(cap) {
        let Some(page) = by_address.get(mv.page.trim()) else {
            report.errors.push(format!(
                "structure: judge named a page not on the list: {}",
                mv.page
            ));
            continue;
        };
        let to = mv.to_wiki.trim();
        if to == page.wiki_id {
            continue;
        }
        match promote::apply_pages_rehome_direct(
            pool,
            tree,
            &page.wiki_id,
            to,
            std::slice::from_ref(&page.page),
            mv.reason.trim(),
            None,
        )
        .await
        {
            Ok(applied) => {
                tracing::info!(
                    page = %mv.page,
                    to_wiki = to,
                    reason = mv.reason.trim(),
                    "rem structure: page re-homed"
                );
                report.applied.push(applied.proposal_id);
            },
            Err(e) => report
                .errors
                .push(format!("structure: {} → {to}: {e}", mv.page)),
        }
    }
}

/// Page-merge sub-job — the **cure front** of semantic page consolidation.
///
/// Structural signals (the reviewer's `duplicate_prose` over the compiled
/// pages, page-name kinship in the persisted plan) **nominate**
/// concept-leaf pairs of the **same wiki** (never an arbitrary wiki pair —
/// gathering one subject's pages into one wiki is the grouping pass's job,
/// and this one sees the result); a dedicated confirmer call (the
/// `rem_dedup_semantic` slot — the low binary-classifier confirmer tier,
/// the same slot the Conciliatore runs on at REM) **confirms**
/// "same concept?" and picks the survivor; the merge then executes
/// **act-first** via [`promote::apply_page_merge_direct`] — every husk fact
/// onto the survivor (re-homing its `wiki_id` when the pair crossed the
/// line), husk file deleted, persisted plan re-homed — with a
/// born-applied receipt the dashboard can show. Capped by [`RemPolicy::page_merge_cap`]
/// confirmation calls per cycle; a pair with any prior page-merge receipt
/// is never re-judged. A failed confirmer call costs its pair and not the
/// night — see [`note_llm_failure`].
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "linear per-pair pipeline (nominate → confirm → execute); splitting hides the order, as in run_auto_promote"
)]
async fn run_page_merge(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    day: &day::DayPerimeter,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
    split_targets: &std::collections::BTreeSet<String>,
    now: DateTime<Utc>,
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
    let duplicate_prose = match reviewer::review(
        tree,
        &plan,
        &reviewer::IdentityContext::default(),
        &now.to_rfc3339(),
    ) {
        Ok(r) => r.duplicate_prose,
        Err(e) => {
            report
                .errors
                .push(format!("merge: reviewer signals unavailable: {e}"));
            Vec::new()
        },
    };
    let scopes = consolidation_scopes(tree, smart_wiki_index)?;
    let agent_family = agent_wikis(&scopes);
    let consolidating = consolidating_wikis(&scopes);
    // The budget counts pairs that reach a JUDGEMENT, not pairs that reach
    // the loop, and it is spent HERE rather than inside `merge_candidates` —
    // that is before the already-judged and settled filters run, so a handful
    // of pairs the operator has already vetoed would fill it, be skipped
    // without a call, and leave every mergeable pair behind them unjudged that
    // night and every night after, with the report saying
    // `candidates_examined: 0` and raising no error.
    let mut budget = policy.page_merge_cap;
    let mut llm_failures = 0usize;
    for (slug_a, slug_b, signal) in
        merge_candidates(&plan, &duplicate_prose, &consolidating, day, split_targets)
    {
        if budget == 0 {
            break;
        }
        // Both slugs come from `merge_candidates`, so the lookups hold.
        let (Some(pa), Some(pb)) = (plan.pages.get(&slug_a), plan.pages.get(&slug_b)) else {
            continue;
        };
        if merge_already_judged(pool, &pa.wiki_id, &pa.page_path, &pb.wiki_id, &pb.page_path)
            .await?
        {
            report.skipped_judged += 1;
            continue;
        }
        let agent = agent_family
            .get(pa.wiki_id.as_str())
            .copied()
            .unwrap_or(false);
        let prompt = merge_prompt(tree, pa, pb, &signal, agent)?;
        let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
        if rem_verdicts::is_settled(pool, rem_verdicts::kind::PAGE_MERGE, &memo_key).await? {
            continue;
        }
        budget -= 1;
        report.candidates_examined += 1;
        let resp = match llm
            .complete(
                CompletionRequest::new(prompt)
                    .with_temperature(0.1)
                    .with_max_tokens(200),
            )
            .await
        {
            Ok(r) => {
                llm_failures = 0;
                r
            },
            Err(e) => {
                let note = format!("page merge failed on {slug_a} vs {slug_b}: {e}");
                if note_llm_failure(&mut report.errors, &mut llm_failures, note) {
                    return Ok(report);
                }
                continue;
            },
        };
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
        let recipient = proposals::recipient_from_fact(
            &husk_rows[0].subject_id,
            husk_rows[0].sender_id.as_ref(),
        );
        let params = PageMergeParams {
            wiki_id: husk.wiki_id.as_str(),
            survivor_wiki_id: survivor.wiki_id.as_str(),
            husk_page: husk.page_path.as_str(),
            survivor_page: survivor.page_path.as_str(),
            fact_ids: &fact_ids,
            husk_title: husk.title.as_str(),
            husk_description: husk.description.as_str(),
            husk_style: husk.style.map(crate::wiki::PageStyle::as_str),
            reason: Some(format!(
                "LLM confirmed same concept ({signal}): {}",
                verdict.reason.as_deref().unwrap_or("no reason given"),
            )),
        };
        match promote::apply_page_merge_direct(pool, tree, &params, recipient.clone()).await {
            Ok(receipt) => {
                wal::complete_rem_op(pool, op_id).await?;
                report.applied.push(receipt.proposal_id.clone());
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
    /// Which of the two ways the item closed: `completed` (the intention was
    /// spent) or `retracted` (it was abandoned). Absent ⇒ `completed`, the
    /// answer this sweep gave when it only knew one of the two.
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    valid_to: Option<String>,
}

impl CompletionItem {
    /// The `decay_reason` this item closes with.
    ///
    /// Only the two the sweep can DISCOVER: an intention is spent or it is
    /// abandoned. `contradicted` is the third closure reason and is not one of
    /// these — it belongs to a fact that was replaced by a truer statement of
    /// the same thing, which the dedup and supersede paths own.
    fn decay_reason(&self) -> &'static str {
        match self.outcome.as_deref() {
            Some(fact_index::decay::RETRACTED) => fact_index::decay::RETRACTED,
            _ => fact_index::decay::COMPLETED,
        }
    }
}

/// One nominated evidence→candidates pairing, ready for the confirmer.
struct CompletionCase<'a> {
    evidence: &'a FactIndexRow,
    candidates: Vec<&'a FactIndexRow>,
}

/// When a fact's sentence existed — the EARLIER of the two clocks the row
/// carries, which is right in all four cases and neither one alone is.
///
/// `created_at` is the row's write instant: correct live, and wrong on a
/// backlog replay, where it is the replay run's wall clock rather than the day
/// the sentence was uttered. `valid_from` is the semantic clock ingest deduces
/// against `occurred_at`: correct on a replay, and wrong when the classifier
/// stamped a real FUTURE start («da luglio lavoro a Milano»), because the
/// engine defines that field as the start of HOLDING, not as the moment of
/// speaking — taking it outright dates a fact to a day that has not happened
/// yet.
///
/// Everything that reasons about the order of the WORLD reads this: what
/// happened before what, whether a plan is still ahead of the evidence that
/// would close it, and which instant a closed window ends on.
fn fact_began(row: &FactIndexRow) -> &str {
    match row.valid_from.as_deref() {
        Some(vf) if vf < row.created_at.as_str() => vf,
        _ => row.created_at.as_str(),
    }
}

/// A claim on one line, cut at `cap` characters with an ellipsis.
fn one_line_capped(text: &str, cap: usize) -> String {
    let one_line = text.replace('\n', " ");
    let mut out: String = one_line.chars().take(cap).collect();
    if one_line.chars().count() > cap {
        out.push('…');
    }
    out
}

/// Short single-line preview of a fact's claim for receipts and logs.
fn fact_preview(text: &str) -> String {
    one_line_capped(text, 120)
}

/// A claim as a **confirmer** must read it: whole, on one line.
///
/// The completion and contradiction sweeps decide by comparing a candidate
/// against the evidence sentence for sentence — a paraphrase is a duplicate
/// and not a completion, a standing condition is not a consumable intention —
/// and a candidate cut at [`fact_preview`]'s length asks for that judgement on
/// half a claim. Facts are short, so the cap here is a runaway guard rather
/// than a budget; [`fact_preview`] stays what a receipt and a log line carry,
/// where a brief quotation is the point.
fn fact_claim(text: &str) -> String {
    one_line_capped(text, CLAIM_FOR_JUDGEMENT_CHARS)
}

/// How much of a claim [`fact_claim`] hands a confirmer.
const CLAIM_FOR_JUDGEMENT_CHARS: usize = 600;

/// Nominate completion cases: fresh evidence facts (created inside
/// `policy.closure_sweep_window`) paired with the most similar
/// OPEN facts of the same wiki (embedding cosine, top 3, older than the
/// evidence).
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
            // only via supersede, tombstone, or its subject's explicit
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
/// (see [`crate::ingest`]); this sub-job catches the rest with the
/// global view: each fresh
/// **evidence** fact is paired with the most similar open items of its
/// wiki (embedding similarity **nominates only** — a resource cap, not
/// a semantic gate), a dedicated LLM call decides what the evidence
/// closed, and the confirmed closures land **act-first**
/// with the same `validity_close` receipt the ingest half writes.
///
/// **Both ways an intention ends.** It is spent (`completed`) or it is
/// abandoned (`retracted`), and the sweep discovers either. The asymmetry
/// mattered: this is the ONLY pass that discovers a closure at all — the
/// contradiction sweep starts from a fact somebody already closed and merely
/// follows its cluster — so a sweep that knew only "it happened" left every
/// cancelled commitment open for ever whenever the turn that cancelled it did
/// not have the commitment in its recall window. The
/// ingest half also notices the affected user, because somebody asked for
/// that closure; a sweep closure is the memory's own bookkeeping and says
/// nothing to anybody.
async fn run_completion_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
    will_be_read: &BTreeSet<&str>,
) -> Result<CompletionSweepReport> {
    let mut report = CompletionSweepReport::default();
    if policy.completion_sweep_cap == 0 {
        return Ok(report);
    }
    // One bucket per standard wiki: an open item is completed by evidence
    // that landed in the same wiki.
    let scopes = consolidation_scopes(tree, smart_wiki_index)?;
    let agent_family = agent_wikis(&scopes);
    let mut by_wiki: BTreeMap<String, Vec<FactIndexRow>> = BTreeMap::new();
    for scope in &scopes {
        let rows = fact_index::find_active_in_wiki(pool, &scope.wiki_id).await?;
        if !rows.is_empty() {
            by_wiki.insert(scope.wiki_id.clone(), rows);
        }
    }
    let cases = completion_cases(&by_wiki, now, policy);
    // The candidate snapshot (`by_wiki`) is built once at the top of the
    // sweep, so two different evidence facts can both nominate the same
    // open item. Track what THIS cycle has already closed and drop those
    // candidates before the confirmer sees them — otherwise the same fact
    // is closed two or three times in one cycle, burning LLM calls and
    // (because `close_validity` has no re-close guard) corrupting the
    // prior-state snapshot of every receipt after the first.
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
        // A request and the fact that finishes it, both on a page the reading
        // will open, is a question that reading already answers
        // ([`all_on_one_page_the_reading_will_open`]).
        if all_on_one_page_the_reading_will_open(
            case.evidence,
            candidates.iter().copied(),
            will_be_read,
        ) {
            continue;
        }
        let case = CompletionCase {
            evidence: case.evidence,
            candidates,
        };
        report.evidence_examined += 1;
        report.candidates_judged += case.candidates.len();
        let agent = agent_family
            .get(case.evidence.wiki_id.as_str())
            .copied()
            .unwrap_or(false);
        match judge_completion_case(pool, tree, llm, cycle_id, &case, agent).await {
            Ok(Some((receipt_ids, closed))) => {
                closed_this_cycle.extend(closed.iter().cloned());
                report.receipts.extend(receipt_ids);
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
    agent_family: bool,
) -> Result<Option<(Vec<String>, Vec<String>)>> {
    let candidates_text = case
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}. {} · {} · {}",
                i + 1,
                c.fact_id.as_str(),
                fact_began(c),
                fact_claim(&c.text)
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
            ("evidence_date", fact_began(case.evidence)),
            ("candidates", candidates_text.as_str()),
            (
                "subject_note",
                if agent_family {
                    AGENT_COMPLETION_NOTE
                } else {
                    ""
                },
            ),
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
    let evidence_began = fact_began(case.evidence);
    let mut applied: Vec<promote::AppliedClosure> = Vec::new();
    // Read once for the sweep, not once per item: it is the same instant for
    // every closure this pass makes ([`fact_index::memory_now`]).
    let memory_now = fact_index::memory_now(pool).await;
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
        // The closing instant: the confirmer's resolved date, else the instant
        // the evidence itself began. The evidence IS what closed the item, so
        // the item ends where its replacement starts; dating it by when the
        // sweep read the evidence puts the closure in the reader's present
        // instead of the fact's.
        let valid_to = item
            .valid_to
            .as_deref()
            .and_then(|b| fact_index::canonical_bound(b, fact_index::DayEdge::End))
            .unwrap_or_else(|| evidence_began.to_owned());
        // The evidence fact IS the successor: it states the outcome the
        // closed fact was waiting for, so the page can point at its home.
        let reason = item.decay_reason();
        // The closure's own instant, for the road the guard takes when the
        // confirmer's date — or the evidence's own start — falls before the
        // item ever began ([`fact_index::end_not_before_start`]). The evidence
        // is what closed the item, so its start is that instant; the wall
        // clock would put a June closure in the September night that read it.
        let closed_when = fact_index::instant_of(evidence_began).unwrap_or(memory_now);
        let Some(prev) = fact_index::close_validity(
            pool,
            &target.fact_id,
            &valid_to,
            reason,
            Some(&case.evidence.fact_id),
            closed_when,
        )
        .await?
        else {
            continue; // vanished between gather and apply
        };
        // The end the write actually stamped, which is not the proposed one
        // when the proposed one fell before the item began
        // ([`fact_index::ClosedValidity::written_valid_to`]).
        let valid_to = prev.written_valid_to.clone();
        tracing::info!(
            fact_id = %target.fact_id,
            evidence = %case.evidence.fact_id,
            valid_to,
            reason,
            "rem completion: validity CLOSED (safety-net sweep)"
        );
        applied.push(promote::AppliedClosure {
            fact_id: target.fact_id.clone(),
            wiki_id: target.wiki_id.clone(),
            preview: fact_preview(&target.text),
            valid_to,
            reason: reason.to_owned(),
            prev,
            surface: promote::ClosureSurface::Fact,
        });
    }
    if applied.is_empty() {
        wal::complete_rem_op(pool, op_id).await?;
        return Ok(None);
    }

    // The same act-first paper trail as the ingest half: a receipt per
    // person whose fact was closed, never one for the batch.
    // The evidence's own text rides in the gesture, so the gesture goes
    // only to the person that evidence belongs to. Everyone else is told
    // that a fact of theirs was closed, and not by what.
    let gesture = format!(
        "REM completion sweep — evidence: {}",
        fact_preview(&case.evidence.text)
    );
    let evidence_recipient =
        proposals::recipient_from_fact(&case.evidence.subject_id, case.evidence.sender_id.as_ref());
    let mut receipts = Vec::new();
    for (recipient, closed) in
        proposals::group_by_recipient(&applied, |c| completion_recipient(case, c))
    {
        let said = (recipient == evidence_recipient).then_some(gesture.as_str());
        let receipt = match promote::emit_validity_close_receipt(
            pool,
            &closed,
            said,
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
        receipts.push(receipt.proposal_id);
    }
    wal::complete_rem_op(pool, op_id).await?;
    let closed = applied
        .iter()
        .map(|c| c.fact_id.as_str().to_owned())
        .collect();
    Ok(Some((receipts, closed)))
}

/// The addressee of ONE closed target: the person its own fact names, and
/// **nobody** when the target is not in the case's candidate list.
///
/// Per target rather than per batch, because the sweep can close facts
/// belonging to several people and each of them gets a receipt of their
/// own ([`proposals::group_by_recipient`]).
///
/// The empty answer is deliberate. Falling back to the evidence's owner
/// would address a receipt about somebody else's fact to them, which is
/// the mistake this whole grouping exists to stop; unaddressed puts it
/// where an unowned receipt belongs. Unreachable today — the closures are
/// built from the candidates — and the same answer the ingest side gives
/// for a closed fact its recall never returned.
fn completion_recipient(
    case: &CompletionCase<'_>,
    closed: &promote::AppliedClosure,
) -> Option<String> {
    case.candidates
        .iter()
        .find(|c| c.fact_id == closed.fact_id)
        .and_then(|c| proposals::recipient_from_fact(&c.subject_id, c.sender_id.as_ref()))
}

// ---------- Page judge sub-job ----------

/// Bundled default for the **page judge** at night, overridable at
/// `<workdir>/prompts/rem-judgement.md`.
pub const BUNDLED_REM_JUDGEMENT_MD: &str = include_str!("../prompts/rem-judgement.md");

/// Bundled default for the **hourly** page judge, overridable at
/// `<workdir>/prompts/rem-judgement-light.md`.
pub const BUNDLED_REM_JUDGEMENT_LIGHT_MD: &str = include_str!("../prompts/rem-judgement-light.md");

/// Longest page rendering one call carries, in bytes.
///
/// A page is normally a few paragraphs; this is the guard against the one that
/// is not, because the whole page goes into the prompt. A rendering that
/// reaches it stops at the last whole fact that fits, prose and listed facts
/// alike, and what is past the cut simply has no marker — so a verdict naming
/// one is refused by name like any other marker that is not there.
const JUDGED_PAGE_CEILING_BYTES: usize = 8_000;

/// How deeply one judgement pass reads.
///
/// **The same reading, twice over, with a different question.** The hour reads
/// the pages its own round touched and asks the three things a page can settle
/// about a fact that just landed on it: is this errand spent with its day,
/// does it finish something the page was still waiting for, is this «trait»
/// really a passage. The night reads every page the day touched and asks two
/// more — is this fact contradicted by one standing beside it, and are two of
/// these the same claim written twice.
///
/// What differs is the question and how many pages each may take. Which model
/// answers is a matter of which SLOT the pass calls, and that belongs to the
/// caller: the night asks the `rem_promotions` slot, the hour the
/// `rem_dedup_semantic` slot it already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgementDepth {
    /// The light dream's pass, over the pages of the round it just wrote.
    Hourly,
    /// The night's pass, over every page the day wrote onto.
    Nightly,
}

impl JudgementDepth {
    /// Name of the prompt this depth renders, and of its operator override in
    /// `<workdir>/prompts/`.
    #[must_use]
    pub const fn prompt_name(self) -> &'static str {
        match self {
            Self::Hourly => "rem-judgement-light",
            Self::Nightly => "rem-judgement",
        }
    }

    /// The bundled default behind [`Self::prompt_name`].
    #[must_use]
    pub const fn bundled_prompt(self) -> &'static str {
        match self {
            Self::Hourly => BUNDLED_REM_JUDGEMENT_LIGHT_MD,
            Self::Nightly => BUNDLED_REM_JUDGEMENT_MD,
        }
    }

    /// How many pages this depth may read in one pass.
    #[must_use]
    pub const fn page_cap(self, policy: &RemPolicy) -> usize {
        match self {
            Self::Hourly => policy.judgement_max_pages_hour,
            Self::Nightly => policy.judgement_max_pages_night,
        }
    }

    /// Whether this depth may act on the two verbs that compare one fact with
    /// another standing beside it.
    ///
    /// The hour is not asked them ([`Self::prompt_name`] renders the shorter
    /// prompt), and this is the engine saying the same thing: a verdict the
    /// hour was not asked for is refused rather than applied, so a model that
    /// answers from habit cannot retire a fact on the cheap reading.
    #[must_use]
    pub const fn weighs_facts_against_each_other(self) -> bool {
        matches!(self, Self::Nightly)
    }

    /// Whether this depth may act on the questions about the PAGE — has it
    /// grown a second subject, where should it lead — rather than only on its
    /// facts.
    ///
    /// The hour corrects a row inside the hour, so that the turn happening now
    /// is answered from a memory that is right: reshaping a page is nobody's
    /// hurry, and it costs the prompt a list of destinations and the pass a
    /// read of the whole plan. One origin for the rule, read where the
    /// question is assembled ([`gather_page`], [`fill_in_the_rails`]) rather
    /// than where it is rendered — so the hour's prompt has no such question in
    /// it, and an answer to one it was not shown finds nothing assembled to
    /// apply.
    ///
    /// Not the same rule as [`Self::weighs_facts_against_each_other`], which
    /// they happen to agree with: that one refuses a VERDICT the hour was not
    /// asked for, and this one decides what the hour's question is built out
    /// of.
    #[must_use]
    pub const fn reshapes_the_page(self) -> bool {
        matches!(self, Self::Nightly)
    }

    /// Which pass this is, for the memo key and the receipt.
    ///
    /// It is part of the memo key because the two passes ask different
    /// questions: a page the hour found nothing on must still be read tonight.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hourly => "hour",
            Self::Nightly => "night",
        }
    }
}

/// One page the judge reads, rendered the way the model sees it.
struct JudgedPage {
    wiki_id: String,
    source_path: String,
    /// The live facts of the page **in marker order**: `facts[i]` is `f{i+1}`.
    facts: Vec<FactIndexRow>,
    /// The page as the model reads it: its prose with the `<fN>` markers, the
    /// facts that are not woven into it yet, and the subject's identity core.
    block: String,
    /// Who the page is about, for the receipt's recipient.
    subject: Option<crate::types::Principal>,
    sender: Option<crate::types::Principal>,
    /// The page inside its own wiki (`cucina.md`), which is how a split
    /// addresses it, and `None` when the path does not sit under a wiki.
    page_in_wiki: Option<String>,
    /// Whether this page is asked about splitting: a depth that reshapes pages
    /// at all ([`JudgementDepth::reshapes_the_page`]), and over the same
    /// deterministic floor the promotion pass reads ([`over_mass_floor`]) — a
    /// resource pre-filter and nothing semantic. False leaves the question out
    /// of the prompt entirely, so an ordinary page is never offered it.
    heavy_enough_to_split: bool,
    /// What this page may be asked about where it leads, or `None` when it is
    /// not asked: the depth does not reshape pages, the page has no page in the
    /// plan, it already carries its full budget of links, or the plan could not
    /// be read at all.
    rails: Option<RailsAsk>,
}

/// What one page needs in front of it to be asked where it should lead.
///
/// Assembled once per pass and not per page ([`fill_in_the_rails`]): the plan
/// and the candidate pool are one read of the whole memory, and a page's own
/// share of them is small.
struct RailsAsk {
    /// This page's slug in the plan — the `from` end of every rail it parks.
    slug: String,
    /// The destinations offered, by slug. A name outside this set was invented
    /// and reaches nothing, so it is dropped by itself and the choices beside
    /// it stand.
    offered: BTreeSet<String>,
    /// How many links the page still has room for
    /// ([`PAGE_RAIL_BUDGET`] minus what it carries).
    budget: usize,
    /// What the model is shown: where this page may lead, and what it already
    /// carries.
    block: String,
    /// What a «nowhere» answer is remembered under, so the rail writer does not
    /// ask this page the same question again ([`rails_memo_subject`]).
    memo_subject: String,
}

impl JudgedPage {
    /// The fact a marker names, when the marker is one of this page's.
    fn fact_at(&self, marker: usize) -> Option<&FactIndexRow> {
        marker
            .checked_sub(1)
            .and_then(|i| self.facts.get(i))
            .filter(|row| row.deleted_at.is_none())
    }

    /// What the MEMO is keyed on: the page as it stands, at this depth.
    ///
    /// Not the rendered prompt — the prose around a fact is rewritten by every
    /// compile that touches the page, and a «nothing to do here» must not be
    /// re-bought because the Cronista chose a different adjective. What the
    /// judge actually reasons about is the facts: which ones, saying what, of
    /// what kind, true until when.
    fn memo_subject(&self, depth: JudgementDepth) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{}|{}", depth.label(), self.source_path);
        for f in &self.facts {
            let _ = write!(
                out,
                "|{}:{}:{}:{}",
                f.fact_id.as_str(),
                f.fact_type.as_deref().unwrap_or(""),
                f.valid_to.as_deref().unwrap_or(""),
                f.text,
            );
        }
        out
    }
}

/// One verdict the judge returned about one marker.
#[derive(Debug, Default, serde::Deserialize)]
struct LlmPageVerdict {
    #[serde(default)]
    verdict: String,
    /// `end` and `retype`: when the fact stops being true.
    #[serde(default)]
    valid_to: Option<String>,
    /// `closes` and `duplicate_of`: the other marker.
    #[serde(default)]
    target: Option<String>,
    /// `contradicted`: the marker that makes this one false.
    #[serde(default)]
    by: Option<String>,
    /// `closes`: `completed` (it was done) or `retracted` (it was called off).
    #[serde(default)]
    reason: Option<String>,
    /// `retype`: the kind the fact should have carried.
    #[serde(default)]
    fact_type: Option<String>,
}

/// The judge's answer about one page: one entry per marker it is changing
/// something about, and the page-level answers beside them.
#[derive(Debug, Default, serde::Deserialize)]
struct PageDecision {
    #[serde(default)]
    verdicts: std::collections::BTreeMap<String, LlmPageVerdict>,
    /// The page has grown two subjects, and these markers are the second one.
    #[serde(default)]
    split: Option<LlmPageSplit>,
    /// Where this page should lead, one entry per fact that raises a question
    /// this page cannot answer.
    #[serde(default)]
    links: Vec<LlmPageLink>,
}

/// One page the judge would divide in two.
#[derive(Debug, serde::Deserialize)]
struct LlmPageSplit {
    /// What the facts that move are about — the new page's name.
    #[serde(default)]
    title: Option<String>,
    /// The markers that go with it.
    #[serde(default)]
    markers: Vec<String>,
}

/// One link the judge would park from this page.
#[derive(Debug, serde::Deserialize)]
struct LlmPageLink {
    /// The marker of the fact that raises the question. A link that names no
    /// fact of this page is dropped: the pass answers a question about one
    /// fact, and an answer that cannot say which fact was not that answer.
    #[serde(default, rename = "for")]
    for_marker: String,
    /// The destination, by slug.
    #[serde(default)]
    to: String,
    /// What the reader is after — it rides the receipt and nothing else.
    #[serde(default)]
    why: String,
}

/// What one verdict did, or why it did nothing.
enum VerdictOutcome {
    /// `(fact_id, verb, detail)`.
    Applied(String, &'static str, String),
    /// `(fact_id, proposal_id)` — the engine wrote nothing and put the
    /// question to the person whose card it is.
    Asked(String, String),
    /// Why the engine would not do what the model asked.
    Refused(&'static str),
    /// The model looked and left the fact alone.
    Kept,
}

/// Whether a fact and everything it is being weighed against sit on ONE page
/// that tonight's reading will open.
///
/// **The reading sees the whole page and already carries this question.** It
/// answers `contradicted` and `closes` about facts it can see together in the
/// prose, so a pair that lives entirely on a page it will read is carried to a
/// model twice: once as a pair, once as a page. This is the one shape the
/// per-fact sweeps can safely leave to it — the moment one of the neighbours
/// lives somewhere else, the reading cannot see it and the sweep is the only
/// pass that can.
///
/// It takes the SELECTION, not the readings: the sweeps run before the reading
/// does, so what is known at that point is which pages it will open. A page
/// whose reading then fails is covered again by the next round, which is a
/// night's delay on a question nobody was asking yesterday.
fn all_on_one_page_the_reading_will_open<'a>(
    seed: &FactIndexRow,
    others: impl IntoIterator<Item = &'a FactIndexRow>,
    will_read: &BTreeSet<&str>,
) -> bool {
    will_read.contains(seed.source_path.as_str())
        && others
            .into_iter()
            .all(|c| c.source_path == seed.source_path)
}

/// The pages a round wrote onto, newest write first, and how many the cap left
/// out.
#[derive(Debug, Default)]
struct PagesToJudge {
    /// Source paths, newest write first — exactly the pages the judge reads.
    pages: Vec<String>,
    /// How many the cap left behind. They are still unjudged, and the next
    /// pass — the next hour's, or tonight's — reads them.
    left: usize,
}

/// Which pages a round wrote onto, capped, newest write first.
///
/// **One origin for the list every other pass keys on.** The reading opens
/// these pages and answers every page-level question about them in one call,
/// and four passes stand aside for that: the completion and contradiction
/// sweeps, which run first and can only know the SELECTION; the per-page
/// split, which runs first for the same reason; and the rail writer, which
/// runs after and keys on the readings that actually landed
/// ([`PageJudgementReport::pages_answered`]). Computed twice the lists could
/// drift apart, and a page in one and not the other is either read twice or
/// not at all.
///
/// The cap belongs here, because what it leaves out is what the *next* pass
/// reads and the count of that is part of the answer. A fact that vanished
/// between the perimeter and here is simply not read; a channel page is
/// nobody's prose and is never judged.
///
/// A page the confirmer memo settles is on this list and skipped by the passes
/// that key on the selection: the answer they want came from the reading the
/// memo stands in for, and nothing about the page has moved since. It never
/// reaches [`PageJudgementReport::pages_answered`], because no answer was
/// bought for it tonight.
async fn pages_the_round_wrote_onto(
    pool: &SqlitePool,
    fresh: &[FactId],
    cap: usize,
) -> PagesToJudge {
    if cap == 0 || fresh.is_empty() {
        return PagesToJudge::default();
    }
    let mut touched: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for id in fresh {
        if let Ok(Some(row)) = fact_index::find_by_id(pool, id).await
            && row.deleted_at.is_none()
            && row.superseded_at.is_none()
            && !crate::wiki::is_channel_page(&row.source_path)
        {
            let at = touched.entry(row.source_path.clone()).or_default();
            if row.created_at > *at {
                at.clone_from(&row.created_at);
            }
        }
    }
    let mut pages: Vec<(String, String)> = touched.into_iter().collect();
    pages.sort_by(|a, b| b.1.cmp(&a.1));
    let left = pages.len().saturating_sub(cap);
    pages.truncate(cap);
    PagesToJudge {
        pages: pages.into_iter().map(|(path, _)| path).collect(),
        left,
    }
}

/// Read the pages a round wrote onto, and correct what the page itself says.
///
/// **Why the page and not the fact.** Every other pass here judges a fact
/// against a handful of candidates fished out by vector distance: the
/// reconciler sees a list of lines, the dedup a pair, the completion and
/// contradiction sweeps a fact plus its nearest neighbours. None of them reads
/// the prose that ties the facts of a page together — which is the thing the
/// Cronista writes and the thing a person would read to answer exactly these
/// questions. This pass reads it: the compiled page, with every live fact
/// marked `<fN>`, and the model answers by marker.
///
/// **What it may change**: a fact's END, a fact's VALIDITY, a fact's KIND, and
/// — at night — which of two identical claims survives. It never deletes,
/// never moves a fact to another subject, never changes who may read it and
/// never rewrites what it says: those are declared verbs a person asks for.
/// And **the identity core is out of reach** ([`the_card_is_out_of_reach`]):
/// a name, a birth date, a relationship changes when a person says it
/// changed. The one correction a card can need is not applied either — it is
/// put to the card's owner as a question
/// ([`proposals::emit_card_retype`]), and silence keeps the card.
///
/// **Newest first.** The pages arrive already chosen and already capped
/// ([`pages_the_round_wrote_onto`]), because the passes that skip them need the
/// same list before this one runs.
///
/// Every change lands act-first, with one receipt per page saying what was
/// applied and what was refused.
///
/// # Errors
///
/// Surfaces infrastructure failures. A model that answers badly is counted in
/// [`PageJudgementReport::errors`], never returned as one.
#[allow(
    clippy::too_many_arguments,
    reason = "one pass: which pages to read, how deeply, by whom, under which policy, at which instant"
)]
async fn run_page_judgement(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llm: Option<&dyn LlmBackend>,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    to_judge: &PagesToJudge,
    depth: JudgementDepth,
) -> Result<PageJudgementReport> {
    let mut report = PageJudgementReport::default();
    let Some(llm) = llm else {
        return Ok(report);
    };
    if to_judge.pages.is_empty() {
        return Ok(report);
    }
    report.pages_left = to_judge.left;

    // Gather, in page order: what the model will be asked, and what the memo
    // says has already been asked about that page as it stands. Reading a page
    // is filesystem and SQL, so it happens here, once, before anything waits
    // on a model.
    let mut asking: Vec<JudgedPage> = Vec::with_capacity(to_judge.pages.len());
    let mut keys: Vec<String> = Vec::with_capacity(to_judge.pages.len());
    for source_path in &to_judge.pages {
        let page = match gather_page(pool, tree, source_path, policy, depth).await {
            Ok(Some(p)) => p,
            // A page with no live facts left has nothing to judge.
            Ok(None) => continue,
            Err(e) => {
                report.errors.push(format!("{source_path}: {e}"));
                continue;
            },
        };
        let key = rem_verdicts::key(llm.model_id(), &page.memo_subject(depth));
        if rem_verdicts::is_settled(pool, rem_verdicts::kind::JUDGEMENT, &key).await? {
            continue;
        }
        asking.push(page);
        keys.push(key);
    }
    report.pages_read = asking.len();
    if depth.reshapes_the_page() {
        fill_in_the_rails(pool, tree, &mut asking).await;
    }

    let mut by_page = ask_about_every_page(tree, llm, &asking, now, depth).await;

    let mut consecutive_failures = 0usize;
    for (i, page) in asking.iter().enumerate() {
        let decision = match by_page[i].take() {
            Some(Ok(d)) => {
                consecutive_failures = 0;
                d
            },
            Some(Err(e)) => {
                // One bad reply costs its page and nothing more; a backend
                // that is down stops the pass with what it has.
                if note_llm_failure(
                    &mut report.errors,
                    &mut consecutive_failures,
                    format!("{}: {e}", page.source_path),
                ) {
                    return Ok(report);
                }
                continue;
            },
            None => continue,
        };
        // The answer came back and could be read: from here on this page HAS
        // been judged, whatever the verdicts in it turned out to be.
        report.pages_answered.push(page.source_path.clone());
        let outcomes = apply_page_decision(
            pool,
            tree,
            &embedder,
            page,
            &decision,
            cycle_id,
            depth,
            llm.model_id(),
            &mut report,
        )
        .await?;
        if outcomes.applied.is_empty() {
            // The page's facts stand exactly as they were read — a question
            // put to somebody is not a change to them — so the same reading of
            // the same page is not worth buying again until something moves.
            rem_verdicts::record_negative(
                pool,
                rem_verdicts::kind::JUDGEMENT,
                &keys[i],
                &page.source_path,
            )
            .await?;
        }
        if !outcomes.worth_a_receipt() {
            continue;
        }
        match emit_page_receipt(pool, page, depth, &outcomes).await {
            Ok(receipt) => report.receipts.push(receipt),
            Err(e) => report
                .errors
                .push(format!("{} receipt: {e}", page.source_path)),
        }
    }
    Ok(report)
}

/// Judge the pages an HOUR just wrote onto, from inside the light dream.
///
/// The night reads every page the day touched and asks five questions of each
/// fact plus two about the page; this reads the pages of the round that has
/// just finished compiling and asks the three a fact can settle on its own
/// ([`JudgementDepth`]). Same rendering, same guards, same receipts — the hour
/// corrects a row inside the hour, it does not reshape a page.
///
/// It runs on the slot the caller hands it — the light round's own
/// `rem_dedup_semantic` — because the operator decides what model that is.
///
/// # Errors
///
/// As [`run_page_judgement`].
pub async fn judge_fresh_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    policy: &RemPolicy,
    fresh: &[FactId],
) -> Result<PageJudgementReport> {
    let now = policy.now.unwrap_or_else(Utc::now);
    let depth = JudgementDepth::Hourly;
    let to_judge = pages_the_round_wrote_onto(pool, fresh, depth.page_cap(policy)).await;
    run_page_judgement(
        pool,
        tree,
        embedder,
        Some(llm),
        cycle_id,
        now,
        policy,
        &to_judge,
        depth,
    )
    .await
}

/// One page's question, in flight: the answer it will carry, and the borrows
/// it holds while it waits.
///
/// Boxed and spelled out rather than left anonymous, because this pass is
/// awaited from a web handler and a stream of anonymous futures over elided
/// lifetimes is the shape the compiler cannot prove `Send` for.
type OnePageAsked<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = (usize, Result<std::result::Result<PageDecision, String>>)>
            + Send
            + 'a,
    >,
>;

/// One page's question, carrying the page's place in the pass so the answer
/// can be put back where it belongs.
///
/// A named function rather than an `async` block inside the stream: the future
/// it returns is one concrete type, which is what lets the whole pass stay
/// `Send` where it is awaited from a web handler.
async fn ask_one_page(
    index: usize,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    page: &JudgedPage,
    now: DateTime<Utc>,
    depth: JudgementDepth,
) -> (usize, Result<std::result::Result<PageDecision, String>>) {
    (index, ask_the_judge(tree, llm, page, now, depth).await)
}

/// Ask the judge about every page of this pass, several at a time, and hand
/// the answers back **in page order**.
///
/// **The asking is what takes the time.** Each page is a separate question
/// about a separate page, so nothing is lost by putting them to the model
/// together — and a night that asked thirty pages one after another spent
/// thirty times one model's latency for no reason. How many really travel at
/// once is the deployment's own limit, which the client enforces call by call
/// ([`crate::llm::install_concurrency_limit`]); the width here matches it, so
/// no prompt is built and then held waiting for a permit.
///
/// **The order comes back.** `buffer_unordered` yields whichever page answers
/// first, and the applying that follows cannot take them that way: the verdicts
/// of one page are applied in marker order against the page as it was READ
/// ([`touches_a_decided_fact`]), and two pages applied at once would be two
/// writers on one `fact_index`. So the answers are put back in the order the
/// pages were chosen, and what a night decided reads in that order too.
async fn ask_about_every_page<'a>(
    tree: &'a WikiTree,
    llm: &'a (dyn LlmBackend + 'a),
    asking: &'a [JudgedPage],
    now: DateTime<Utc>,
    depth: JudgementDepth,
) -> Vec<Option<std::result::Result<PageDecision, String>>> {
    let mut by_page: Vec<Option<std::result::Result<PageDecision, String>>> =
        (0..asking.len()).map(|_| None).collect();
    // Boxed, and with the borrows spelled out: the pass is awaited from a web
    // handler, and a stream of anonymous futures over elided lifetimes is
    // exactly the shape the compiler cannot prove `Send` for.
    let pending: Vec<OnePageAsked<'a>> = asking
        .iter()
        .enumerate()
        .map(|(i, page)| Box::pin(ask_one_page(i, tree, llm, page, now, depth)) as OnePageAsked<'a>)
        .collect();
    let answers: Vec<(usize, Result<std::result::Result<PageDecision, String>>)> =
        futures_util::stream::iter(pending)
            .buffer_unordered(crate::llm::in_flight_width())
            .collect::<Vec<_>>()
            .await;
    for (i, outcome) in answers {
        by_page[i] = Some(match outcome {
            Ok(answer) => answer,
            // A prompt that will not render is the same failure for every page
            // of this pass, and it is the operator's to fix.
            Err(e) => Err(format!("{e}")),
        });
    }
    by_page
}

/// Build one page's reading: its live facts in marker order, the prose that
/// ties them together, and the identity core of whoever the page is about.
///
/// `None` when the page has no live fact left — there is nothing to judge and
/// nothing to pay for.
async fn gather_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    source_path: &str,
    policy: &RemPolicy,
    depth: JudgementDepth,
) -> Result<Option<JudgedPage>> {
    let live: Vec<FactIndexRow> = fact_index::find_active_by_source_path(pool, source_path)
        .await
        .unwrap_or_default();
    if live.is_empty() {
        return Ok(None);
    }
    // The page's subject: the one most of its facts are about. A page holds
    // one subject in the ordinary case, and the identity core that answers
    // «is this really a trait» is that person's.
    let mut tally: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in &live {
        *tally.entry(f.subject_id.to_string()).or_default() += 1;
    }
    let subject = tally
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .and_then(|(s, _)| s.parse::<crate::types::Principal>().ok());
    let card: Vec<FactIndexRow> = match &subject {
        Some(s) => fact_index::find_active_by_subject(pool, s)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(FactIndexRow::is_identity_core)
            .collect(),
        None => Vec::new(),
    };

    let raw = std::fs::read_to_string(tree.workdir().join(source_path)).unwrap_or_default();
    let body = crate::wiki::MarkdownDoc::parse(&raw).map_or_else(|| raw.clone(), |doc| doc.body);
    let (prose, facts) = render_page_with_markers(&body, live);
    let sender = facts.first().and_then(|f| f.sender_id.clone());
    let wiki_id = facts.first().map(|f| f.wiki_id.clone()).unwrap_or_default();

    let mut block = prose;
    if !card.is_empty() {
        block.push_str("\n\nWHO THIS PAGE IS ABOUT — their identity card:\n");
        for f in &card {
            block.push_str("- ");
            block.push_str(&one_line_capped(&f.text, 160));
            block.push('\n');
        }
    }
    // Is this page asked about a split, and what is it called inside its own
    // wiki? Both come from the wiki the page sits in, and a page whose wiki
    // cannot be located is simply never split.
    let (page_in_wiki, heavy_enough_to_split) = tree
        .walk()
        .ok()
        .and_then(|wikis| {
            let d = wikis
                .into_iter()
                .find(|d| d.meta.wiki_id.as_str() == wiki_id)?;
            let rel = wiki_relative_page(&d, source_path)?;
            // The card is never split — recall serves it whole into every turn
            // — and neither is a channel page, whose reader keys on the path.
            let splittable = depth.reshapes_the_page()
                && !wiki::is_identity_card_page(source_path)
                && !wiki::is_channel_page(source_path)
                && over_mass_floor(&d, source_path, facts.len(), policy);
            Some((Some(rel), splittable))
        })
        .unwrap_or((None, false));

    Ok(Some(JudgedPage {
        wiki_id,
        source_path: source_path.to_owned(),
        facts,
        block,
        subject,
        sender,
        page_in_wiki,
        heavy_enough_to_split,
        // Filled once for the whole pass, from the plan and the candidate
        // pool — neither of which is worth reading per page.
        rails: None,
    }))
}

/// Give each gathered page what it needs to be asked where it should lead.
///
/// **Once for the pass, never per page.** The plan is one file and the
/// candidate pool one read of the whole memory; a page's own share of them is a
/// handful of destinations. A plan that cannot be read is a night where no page
/// is asked this — nothing here is a repair, only an addition.
///
/// The same two fences the rail writer stands on, for the same reasons: a
/// reserved page is never asked (the compiler would refuse what it answered),
/// and an identity card is never a destination.
async fn fill_in_the_rails(pool: &SqlitePool, tree: &WikiTree, asking: &mut [JudgedPage]) {
    let plan = match crate::planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "page judge: plan unreadable — no page is asked where it leads");
            return;
        },
    };
    let mut slug_of: BTreeMap<String, &str> = BTreeMap::new();
    let mut destinations: BTreeMap<String, String> = BTreeMap::new();
    for (slug, p) in &plan.pages {
        let Some(path) = crate::planner::plan_page_source_path(tree, p) else {
            continue;
        };
        if !p.is_identity_card() {
            destinations.insert(path.clone(), slug.clone());
        }
        slug_of.insert(path, slug.as_str());
    }

    // Which of these pages has room for a link at all. The plan alone answers
    // it, and on a night where none has, the candidate pool — two reads of the
    // whole memory — is never loaded.
    let eligible: Vec<(usize, &str, usize)> = asking
        .iter()
        .enumerate()
        .filter(|(_, page)| {
            !crate::wiki::names_reserved_page(std::path::Path::new(&page.source_path))
        })
        .filter_map(|(i, page)| {
            let slug = slug_of.get(&page.source_path).copied()?;
            let carried = plan.link_graph.get(slug).map_or(0, Vec::len);
            let budget = PAGE_RAIL_BUDGET.checked_sub(carried).filter(|b| *b > 0)?;
            Some((i, slug, budget))
        })
        .collect();
    if eligible.is_empty() {
        return;
    }
    let candidates = crate::candidates::CandidatePool::load(pool, &destinations).await;

    for (i, slug, budget) in eligible {
        let Some(plan_page) = plan.pages.get(slug) else {
            continue;
        };
        let offered = rail_candidates(pool, &plan, &candidates, slug, plan_page).await;
        let (block, offered) = rails_block(&plan, slug, &offered, budget);
        if offered.is_empty() {
            continue;
        }
        asking[i].rails = Some(RailsAsk {
            memo_subject: rails_memo_subject(slug, plan_page),
            slug: slug.to_owned(),
            offered,
            budget,
            block,
        });
    }
}

/// What one page is shown beneath [`RAILS_QUESTION`], and the set an answer is
/// held to: where it may lead, where it already leads, and how much room is
/// left.
///
/// **The list shown and the list accepted are one list**, built here in one
/// pass, because a destination the model cannot see but the engine would take
/// is a fence that lets through what it never offered.
///
/// The destinations it already points at are not among the offered ones — the
/// selection excludes them — and they are named anyway, so an answer is not
/// spent proposing a link the page carries.
fn rails_block(
    plan: &CompilationPlan,
    slug: &str,
    offered: &[crate::candidates::Candidate],
    budget: usize,
) -> (String, BTreeSet<String>) {
    use std::fmt::Write as _;

    let mut destinations = String::new();
    let mut names: BTreeSet<String> = BTreeSet::new();
    for c in offered {
        let Some(p) = plan.pages.get(&c.key) else {
            continue;
        };
        let _ = writeln!(destinations, "- {}: {}", c.key, p.description);
        names.insert(c.key.clone());
    }
    let carried = plan
        .link_graph
        .get(slug)
        .filter(|l| !l.is_empty())
        .map_or_else(|| "nowhere yet".to_owned(), |ls| ls.join(", "));
    let block = format!(
        "WHERE THIS PAGE MAY LEAD — name one of these and nothing else:\n{destinations}\n\
         IT ALREADY LEADS TO: {carried}\n\
         ROOM FOR: {budget} more link(s), and fewer is the better answer.\n"
    );
    (block, names)
}

/// Render a page the way the judge reads it: the Cronista's prose with each
/// live fact wrapped in `<fN>`, and then the facts that are not in the prose
/// yet, one per line under their own heading.
///
/// The walk itself is [`crate::marked_page::mark_up`], shared with the turn's
/// reconciler — the two ask different questions of a page and both ask them of
/// the same rendering. Here the reader is the ENGINE, so a region whose fact
/// this pass did not list keeps its words as the scaffolding they are.
///
/// **The second half is the declared fallback.** A fact reaches its page's
/// prose only at the compile that follows its writing, and a page compiled
/// after this pass — or one whose compile failed — would otherwise hide its
/// newest facts from the only pass that reads them. So every live fact gets a
/// marker: the woven ones in place, the rest listed. A page with no prose at
/// all degrades to the list, which is exactly the old fact-by-fact reading.
///
/// Returns the rendering and the facts in marker order.
fn render_page_with_markers(body: &str, live: Vec<FactIndexRow>) -> (String, Vec<FactIndexRow>) {
    use std::fmt::Write as _;

    let ids: Vec<&str> = live.iter().map(|f| f.fact_id.as_str()).collect();
    let marked = crate::marked_page::mark_up(
        body,
        &ids,
        crate::marked_page::UnknownRegions::Keep,
        JUDGED_PAGE_CEILING_BYTES,
        1,
    );

    let mut loose = String::new();
    for (position, i) in marked.order.iter().enumerate().skip(marked.woven) {
        let n = position + 1;
        let f = &live[*i];
        let _ = writeln!(
            loose,
            "<f{n}>{}</f{n}> · {} · until {}",
            one_line_capped(&f.text, 300),
            f.fact_type.as_deref().unwrap_or("(no kind)"),
            f.valid_to.as_deref().unwrap_or("(no end)"),
        );
    }
    let mut out = String::new();
    if marked.woven > 0 {
        out.push_str("THE PAGE:\n");
        out.push_str(&marked.prose);
        out.push('\n');
    }
    if !loose.is_empty() {
        out.push_str("\nON THIS PAGE, NOT WOVEN INTO THE PROSE YET:\n");
        out.push_str(&loose);
    }
    // The facts in the order their markers were handed out.
    let facts = {
        let mut slots: Vec<Option<FactIndexRow>> = live.into_iter().map(Some).collect();
        marked
            .order
            .into_iter()
            .filter_map(|i| slots[i].take())
            .collect()
    };
    (out, facts)
}

/// The question about splitting, spliced into the night's prompt for a page
/// that is heavy enough to be asked it.
///
/// **It rides with the rest because it is the same reading.** Splitting a page
/// is a judgement about what the page IS — whether one subject has grown
/// inside another — and the reader who has just decided what each fact on it
/// says is the one who can answer it. Asked on its own it is a second call
/// carrying the same page, which is why the pass that asks it that way
/// ([`run_auto_promote`]) skips the pages this one reads.
const SPLIT_QUESTION: &str = "\
## THE PAGE: HAS IT GROWN A SECOND SUBJECT?  →  `split`\n\
\n\
This page carries enough facts to be worth the question: is what it holds ONE \
subject, or has a second one grown inside it? A page is one thing a reader \
came for; when a run of its facts is plainly about something else — one \
person's illness inside a household page, a renovation inside a kitchen page — \
those facts have outgrown their siblings and want a page of their own.\n\
\n\
`{\"split\": {\"title\": \"what the ones that move are about\", \"markers\": \
[\"f3\", \"f4\"]}}` — name the markers that MOVE, and nothing else. The engine \
opens the page, moves those facts and leaves the rest where they are.\n\
\n\
- ❌ **Moving everything is a rename, not a split.** Leave at least one fact \
behind, or answer `null`.\n\
- ❌ A page that is long but is one subject stays whole. Length is not the \
question; TWO SUBJECTS is.\n\
- ❌ A list, a page of dated entries, a record of measurements: each is one \
thing by construction, however many rows it has.\n\
- ✅ `null` — and most pages are `null`.\n";

/// The question about where a page should lead, spliced into the night's
/// prompt for a page that still has room for a link.
///
/// **It rides with the rest because it is the same reading.** What a reader who
/// has just met this fact needs next is answered by whoever has just read the
/// fact in its page's prose — and that reading is already bought. Asked on its
/// own it is a second call carrying the same page's facts, which is why the
/// pass that asks it that way ([`run_rail_writer`]) skips the pages this one
/// reads.
///
/// The destinations, the budget and the links the page carries are appended per
/// page ([`RailsAsk::block`]): the question is the same for every page, and what
/// it may answer is not.
const RAILS_QUESTION: &str = "\
## THE PAGE: WHICH OF ITS FACTS LEAVES A READER STUCK?  →  `links`\n\
\n\
Somebody reading this page meets a fact and wants the rest of that story — and \
from here there is no way to it. Name the fact, and the page that would answer \
it. A link is a MANDATORY sentence at this page's next rewrite, so a link \
nobody needs costs a clause of prose for ever.\n\
\n\
`{\"links\": [{\"for\": \"f3\", \"to\": \"<a page from the list below>\", \
\"why\": \"what the reader is after\"}]}` — the marker of the fact that raises \
the question, and the page that answers it.\n\
\n\
- ❌ **Only the pages listed below.** A page you name that is not there reaches \
nothing and is dropped.\n\
- ❌ A page that merely resembles this one. Two pages about the same subject do \
not need to point at each other; a reader who wants one is already on it.\n\
- ❌ A link for a fact that raises no question. Most facts raise none.\n\
- ✅ `[]` — and most pages are `[]`.\n";

/// Ask the judge about one page.
async fn ask_the_judge(
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    page: &JudgedPage,
    now: DateTime<Utc>,
    depth: JudgementDepth,
) -> Result<std::result::Result<PageDecision, String>> {
    // No language directive: the answer is markers and enum words, never
    // anything a person reads, so steering its language is tokens paid for
    // nothing (`prompts::PromptOutput::Internal`).
    //
    // The two questions about the page itself are rendered for a page that
    // qualifies for each — heavy enough to hold two subjects
    // ([`JudgedPage::heavy_enough_to_split`], the promotion pass's own floor),
    // short enough of links to have room for one more ([`JudgedPage::rails`]).
    // Both were decided where the question was assembled, depth included, so a
    // page that qualifies for neither carries neither in its prompt and the
    // model is never offered a decision nobody wants from it.
    const NOT_ASKED: &str = "(not asked of this page)";
    let splitting = if page.heavy_enough_to_split {
        SPLIT_QUESTION
    } else {
        NOT_ASKED
    };
    let rails = page.rails.as_ref().map_or_else(
        || NOT_ASKED.to_owned(),
        |ask| format!("{RAILS_QUESTION}\n{}", ask.block),
    );
    let prompt = prompts::render(
        depth.prompt_name(),
        tree.workdir(),
        depth.bundled_prompt(),
        &[
            ("now", fact_index::bound_from_instant(now).as_str()),
            ("page", page.block.as_str()),
            ("splitting", splitting),
            ("rails", rails.as_str()),
        ],
    )?;
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                // Room for the verdicts AND the page's own answers riding with
                // them: one call carries what three questions ask.
                .with_max_tokens(4_000),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(Err(format!("{e}"))),
    };
    let Some(raw) = first_json_object(&resp.text) else {
        return Ok(Err("no JSON object in the answer".to_owned()));
    };
    Ok(serde_json::from_value(raw).map_err(|e| format!("answer shape: {e}")))
}

/// Apply one page's verdicts in marker order, act-first.
#[allow(
    clippy::too_many_arguments,
    reason = "one page's verdicts: what was decided, about which page, by whom, how deeply"
)]
async fn apply_page_decision(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    page: &JudgedPage,
    decision: &PageDecision,
    cycle_id: &str,
    depth: JudgementDepth,
    model_id: &str,
    report: &mut PageJudgementReport,
) -> Result<PageOutcomes> {
    let mut outcomes = PageOutcomes::default();
    let mut ordered: Vec<(usize, &String, &LlmPageVerdict)> = decision
        .verdicts
        .iter()
        .filter_map(|(marker, v)| crate::marked_page::marker_number(marker).map(|n| (n, marker, v)))
        .collect();
    ordered.sort_by_key(|(n, _, _)| *n);
    // One fact, one decision per reading. The verdicts are applied against the
    // page as it was READ, so a second verdict reaching the same fact — two
    // markers both closing `f1`, a fact merged and then ended — would be
    // deciding from a picture that is already out of date. The engine takes
    // the first and says so about the rest.
    let mut decided: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Read once for the page, not once per verdict: it is the same instant for
    // every fact on it ([`fact_index::memory_now`]).
    let memory_now = fact_index::memory_now(pool).await;
    for (marker, spelling, verdict) in ordered {
        let outcome = match page.fact_at(marker) {
            None => VerdictOutcome::Refused("no such marker on this page"),
            Some(row) if touches_a_decided_fact(page, verdict, row, &decided) => {
                VerdictOutcome::Refused("that fact was already decided in this reading")
            },
            Some(row) => {
                apply_one_page_verdict(
                    pool, tree, embedder, page, row, verdict, cycle_id, memory_now, depth,
                )
                .await?
            },
        };
        match outcome {
            VerdictOutcome::Applied(fact_id, verb, detail) => {
                decided.insert(fact_id.clone());
                report.changed.push(format!("{fact_id} · {verb}"));
                outcomes.applied.push(serde_json::json!({
                    "marker": spelling,
                    "fact_id": fact_id,
                    "verb": verb,
                    "detail": detail,
                }));
            },
            VerdictOutcome::Asked(fact_id, proposal_id) => {
                // Nothing was written, so the fact is not «decided»: what
                // stands in the way of a second verdict about it is the
                // pending question itself.
                report.asked.push(format!("{fact_id} · {proposal_id}"));
                outcomes.asked.push(serde_json::json!({
                    "marker": spelling,
                    "fact_id": fact_id,
                    "verb": "retype",
                    "detail": format!("asked the card's owner ({proposal_id})"),
                }));
            },
            VerdictOutcome::Refused(why) => {
                report
                    .refused
                    .push(format!("{}:{spelling} · {why}", page.source_path));
                tracing::info!(
                    page = page.source_path,
                    marker = spelling,
                    verdict = verdict.verdict,
                    reason = why,
                    "page judge: verdict refused"
                );
                outcomes.refused.push(serde_json::json!({
                    "marker": spelling,
                    "verdict": verdict.verdict,
                    "reason": why,
                }));
            },
            VerdictOutcome::Kept => {},
        }
    }

    // The page's own answers, after the facts.
    //
    // Where a page leads is decided on the page the model read, so it is parked
    // before the split moves anything off: a link is from this page to another,
    // and it stands whichever of this page's facts raised it.
    apply_page_rails(
        pool,
        tree,
        page,
        &decision.links,
        cycle_id,
        model_id,
        report,
        &mut outcomes,
    )
    .await?;

    // The split runs last: it moves rows off this page, so every verdict about
    // them has landed by the time it does.
    if let Some(split) = &decision.split
        && !split.markers.is_empty()
    {
        record_page_split(pool, tree, page, split, cycle_id, report, &mut outcomes).await?;
    }
    Ok(outcomes)
}

/// Carry out the split the reading asked for, and write down what it did or
/// why the engine would not do it.
async fn record_page_split(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &JudgedPage,
    split: &LlmPageSplit,
    cycle_id: &str,
    report: &mut PageJudgementReport,
    outcomes: &mut PageOutcomes,
) -> Result<()> {
    match apply_page_split(pool, tree, page, split, cycle_id).await? {
        VerdictOutcome::Applied(what, verb, detail) => {
            report.changed.push(format!("{what} · {verb}"));
            outcomes.applied.push(serde_json::json!({
                "marker": "(the page)",
                "fact_id": what,
                "verb": verb,
                "detail": detail,
            }));
        },
        VerdictOutcome::Refused(why) => {
            report
                .refused
                .push(format!("{}:split · {why}", page.source_path));
            outcomes.refused.push(serde_json::json!({
                "marker": "(the page)",
                "verdict": "split",
                "reason": why,
            }));
        },
        VerdictOutcome::Asked(..) | VerdictOutcome::Kept => {},
    }
    Ok(())
}

/// What one page's reading came to: what the engine did, what it put to a
/// person instead, and what it would not do.
#[derive(Debug, Default)]
struct PageOutcomes {
    applied: Vec<serde_json::Value>,
    asked: Vec<serde_json::Value>,
    refused: Vec<serde_json::Value>,
}

impl PageOutcomes {
    /// Whether this reading did anything worth a receipt. A page where every
    /// verdict was `keep` — or where the only answer was one the engine
    /// refused — leaves no row: the refusal is in the log and the page is
    /// exactly as it was.
    const fn worth_a_receipt(&self) -> bool {
        !self.applied.is_empty() || !self.asked.is_empty()
    }
}

/// Whether this pass may do this to a fact of somebody's identity core.
///
/// **Nothing on the identity core is closed, ended or retired while everybody
/// is asleep.** It is the rule [`FactIndexRow::is_identity_core`] states and
/// the dedup revisor already holds to: a name, a birth date, a relationship, a
/// standing health constraint changes when a person says it changed, and never
/// because a night read a page and thought it saw something.
///
/// The one thing the judge may do about such a fact is ASK to take it off the
/// card — `retype`, the verb that says «this reads as who somebody IS and is
/// really a passage of some months», which on card material opens a question
/// to its owner rather than writing anything
/// ([`proposals::emit_card_retype`]). That is the whole reason the card is
/// offered to this pass at all, so it is the one exception here, and the other
/// four verbs are refused by name on the receipt.
fn the_card_is_out_of_reach(row: &FactIndexRow, verb: &str) -> bool {
    row.is_identity_core() && verb != "retype"
}

/// Whether this verdict would act on a fact an earlier verdict of the same
/// reading already changed — the judged fact itself, or the other fact it
/// names.
fn touches_a_decided_fact(
    page: &JudgedPage,
    verdict: &LlmPageVerdict,
    row: &FactIndexRow,
    decided: &std::collections::BTreeSet<String>,
) -> bool {
    if decided.contains(row.fact_id.as_str()) {
        return true;
    }
    [verdict.target.as_deref(), verdict.by.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(crate::marked_page::marker_number)
        .filter_map(|n| page.fact_at(n))
        .any(|other| decided.contains(other.fact_id.as_str()))
}

/// Apply ONE verdict about one marker.
///
/// Every arm ends the same way: the engine does what the model asked only when
/// what it asked is possible, and says by name why not when it is not. The
/// refusals are the interesting half — they are what a person reads on the
/// receipt when the judge was wrong.
#[allow(
    clippy::too_many_arguments,
    reason = "one verdict: what, about which fact, on which page, by whom, when, how deeply"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one arm per verb, in the order the prompt asks them: splitting them hides that this is the whole list"
)]
async fn apply_one_page_verdict(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    page: &JudgedPage,
    row: &FactIndexRow,
    verdict: &LlmPageVerdict,
    cycle_id: &str,
    memory_now: DateTime<Utc>,
    depth: JudgementDepth,
) -> Result<VerdictOutcome> {
    // The instant a judgement acts on is the FACT's own, not the clock the
    // pass runs on: a night catching up on a backlog is not the day the errand
    // happened ([`fact_index::end_not_before_start`]). Which of the row's two
    // clocks that is, is [`fact_began`]'s answer and not this function's — on
    // a replay `created_at` is the run's own evening — and where the row
    // carries neither readably, the instant the memory is at
    // ([`fact_index::memory_now`]).
    let said_at = fact_index::instant_of(fact_began(row)).unwrap_or(memory_now);
    match verdict.verdict.trim() {
        "keep" | "" => Ok(VerdictOutcome::Kept),

        // The identity core, before anything else is considered.
        verb if the_card_is_out_of_reach(row, verb) => Ok(VerdictOutcome::Refused(
            "a fact on somebody's identity card is only ever RETYPED, never closed",
        )),

        // 1 — an errand, true of its day and of no other.
        "end" => {
            if row.valid_to.is_some() {
                return Ok(VerdictOutcome::Refused("it already ends"));
            }
            if fact_index::is_an_identity_kind(row.fact_type.as_deref()) {
                return Ok(VerdictOutcome::Refused(
                    "a fact on the identity card is retyped, never ended",
                ));
            }
            let day = fact_index::instant_of(
                row.valid_from.as_deref().unwrap_or(row.created_at.as_str()),
            )
            .unwrap_or(said_at);
            let end = verdict
                .valid_to
                .as_deref()
                .and_then(|raw| fact_index::canonical_bound(raw, fact_index::DayEdge::End))
                .unwrap_or_else(|| {
                    fact_index::canonical_bound(
                        &day.format("%Y-%m-%d").to_string(),
                        fact_index::DayEdge::End,
                    )
                    .unwrap_or_else(|| fact_index::bound_from_instant(day))
                });
            let op = wal::begin_rem_op(
                pool,
                cycle_id,
                "judgement_end",
                Some(page.wiki_id.as_str()),
                None,
            )
            .await?;
            let written =
                fact_index::set_validity(pool, &row.fact_id, None, Some(end.as_str())).await?;
            wal::complete_rem_op(pool, op).await?;
            match written {
                fact_index::ValidityEdit::Applied(_) => Ok(VerdictOutcome::Applied(
                    row.fact_id.as_str().to_owned(),
                    "end",
                    end,
                )),
                fact_index::ValidityEdit::EndBeforeStart => Ok(VerdictOutcome::Refused(
                    "that end comes before the fact began",
                )),
                fact_index::ValidityEdit::NoSuchRow => {
                    Ok(VerdictOutcome::Refused("the fact is gone"))
                },
            }
        },

        // 2 — this fact finishes something the page was still waiting for.
        "closes" => {
            let Some(target) = verdict
                .target
                .as_deref()
                .and_then(crate::marked_page::marker_number)
            else {
                return Ok(VerdictOutcome::Refused("no marker to close"));
            };
            let Some(open) = page.fact_at(target) else {
                return Ok(VerdictOutcome::Refused("no such marker on this page"));
            };
            if open.fact_id == row.fact_id {
                return Ok(VerdictOutcome::Refused("a fact does not close itself"));
            }
            if open.valid_to.is_some() {
                return Ok(VerdictOutcome::Refused("it is already closed"));
            }
            // The same rule as above, asked of the fact this verb actually
            // closes: here it is the TARGET that would be retired, not the
            // fact the verdict is about.
            if the_card_is_out_of_reach(open, "closes") {
                return Ok(VerdictOutcome::Refused(
                    "a fact on somebody's identity card is only ever RETYPED, never closed",
                ));
            }
            let reason = match verdict.reason.as_deref() {
                Some("retracted") => fact_index::decay::RETRACTED,
                _ => fact_index::decay::COMPLETED,
            };
            let op = wal::begin_rem_op(
                pool,
                cycle_id,
                "judgement_closes",
                Some(open.wiki_id.as_str()),
                None,
            )
            .await?;
            let closed = fact_index::close_validity(
                pool,
                &open.fact_id,
                &fact_index::bound_from_instant(said_at),
                reason,
                Some(&row.fact_id),
                said_at,
            )
            .await?;
            wal::complete_rem_op(pool, op).await?;
            match closed {
                Some(prev) => Ok(VerdictOutcome::Applied(
                    open.fact_id.as_str().to_owned(),
                    "closes",
                    format!("{reason} at {}", prev.written_valid_to),
                )),
                None => Ok(VerdictOutcome::Refused("the fact is gone")),
            }
        },

        // 3 — a passage wearing the clothes of a trait.
        "retype" => {
            if !fact_index::is_an_identity_kind(row.fact_type.as_deref()) {
                return Ok(VerdictOutcome::Refused(
                    "only a fact filed as who somebody IS is retyped",
                ));
            }
            if verdict.fact_type.as_deref() != Some("state") {
                return Ok(VerdictOutcome::Refused(
                    "the judge files a passage as `state`, and writes no other kind",
                ));
            }
            let end = verdict
                .valid_to
                .as_deref()
                .and_then(|raw| fact_index::canonical_bound(raw, fact_index::DayEdge::End));

            // **On the identity core this is a question, not an act.** Off the
            // page, «this really was a passage» and «the model misread a
            // trait» look the same, and one of them is somebody's own record
            // rewritten while they slept. So the card's owner is asked, the
            // engine writes nothing, and silence keeps the card as it is
            // ([`proposals::emit_card_retype`]). Elsewhere — a `bio` fact that
            // is not always-on card material — the correction is ordinary and
            // lands act-first like the rest.
            if row.is_identity_core() {
                let asking = proposals::CardRetype {
                    fact_id: row.fact_id.clone(),
                    subject: row.subject_id.clone(),
                    text: one_line_capped(&row.text, 300),
                    ends: end.clone(),
                    source_path: row.source_path.clone(),
                    wiki_id: row.wiki_id.clone(),
                };
                return Ok(proposals::emit_card_retype(pool, &asking).await?.map_or(
                    VerdictOutcome::Refused(
                        "the card's owner has already been asked about this line",
                    ),
                    |proposal_id| {
                        VerdictOutcome::Asked(row.fact_id.as_str().to_owned(), proposal_id)
                    },
                ));
            }
            let op = wal::begin_rem_op(
                pool,
                cycle_id,
                "judgement_retype",
                Some(page.wiki_id.as_str()),
                None,
            )
            .await?;
            let changed =
                fact_index::set_fact_type(pool, &row.fact_id, "state", end.as_deref(), said_at)
                    .await?;
            wal::complete_rem_op(pool, op).await?;
            let Some(prev) = changed else {
                return Ok(VerdictOutcome::Refused("the fact is gone"));
            };
            // What the receipt says happened, in the two things that changed.
            let prev_kind = prev.prev_fact_type.as_deref().unwrap_or("(no kind)");
            let detail = match (&end, prev.prev_valid_to.as_deref()) {
                (Some(ends), Some(replaced)) if ends != replaced => {
                    format!("{prev_kind} → state, until {ends} (replacing {replaced})")
                },
                (Some(ends), _) => format!("{prev_kind} → state, until {ends}"),
                (None, _) => format!("{prev_kind} → state, no end stated"),
            };
            Ok(VerdictOutcome::Applied(
                row.fact_id.as_str().to_owned(),
                "retype",
                detail,
            ))
        },

        // 4 — something standing on the page makes it false.
        "contradicted" => {
            if !depth.weighs_facts_against_each_other() {
                return Ok(VerdictOutcome::Refused(
                    "this pass was not asked to weigh two facts against each other",
                ));
            }
            let Some(by) = verdict
                .by
                .as_deref()
                .and_then(crate::marked_page::marker_number)
            else {
                return Ok(VerdictOutcome::Refused(
                    "no marker said what makes it false",
                ));
            };
            let Some(against) = page.fact_at(by) else {
                return Ok(VerdictOutcome::Refused("no such marker on this page"));
            };
            if against.fact_id == row.fact_id {
                return Ok(VerdictOutcome::Refused("a fact does not contradict itself"));
            }
            if row.valid_to.is_some() {
                return Ok(VerdictOutcome::Refused("it is already closed"));
            }
            // The contradiction happened when the CONTRARY fact did, when that
            // is the later of the two; never at the clock this pass runs on.
            let when = fact_index::instant_of(
                against
                    .valid_from
                    .as_deref()
                    .unwrap_or(against.created_at.as_str()),
            )
            .unwrap_or(said_at);
            let op = wal::begin_rem_op(
                pool,
                cycle_id,
                "judgement_contradicted",
                Some(page.wiki_id.as_str()),
                None,
            )
            .await?;
            let closed = fact_index::close_validity(
                pool,
                &row.fact_id,
                &fact_index::bound_from_instant(when),
                fact_index::decay::CONTRADICTED,
                Some(&against.fact_id),
                when,
            )
            .await?;
            wal::complete_rem_op(pool, op).await?;
            match closed {
                Some(prev) => Ok(VerdictOutcome::Applied(
                    row.fact_id.as_str().to_owned(),
                    "contradicted",
                    prev.written_valid_to,
                )),
                None => Ok(VerdictOutcome::Refused("the fact is gone")),
            }
        },

        // 5 — the same claim, written twice on one page.
        "duplicate_of" => {
            if !depth.weighs_facts_against_each_other() {
                return Ok(VerdictOutcome::Refused(
                    "this pass was not asked to weigh two facts against each other",
                ));
            }
            let Some(target) = verdict
                .target
                .as_deref()
                .and_then(crate::marked_page::marker_number)
            else {
                return Ok(VerdictOutcome::Refused("no marker to merge with"));
            };
            let Some(twin) = page.fact_at(target) else {
                return Ok(VerdictOutcome::Refused("no such marker on this page"));
            };
            if twin.fact_id == row.fact_id {
                return Ok(VerdictOutcome::Refused("a fact is not its own duplicate"));
            }
            // Which survives is not the model's to choose, and never has been:
            // the newer copy wins, as in the dedup revisor, because it carries
            // whatever the later turn added — and the check below is what makes
            // that sentence true rather than assumed.
            let (winner, loser) = if row.created_at >= twin.created_at {
                (row, twin)
            } else {
                (twin, row)
            };
            if loser.is_identity_core() {
                return Ok(VerdictOutcome::Refused(
                    "a fact on somebody's identity core is never retired in the background",
                ));
            }
            // **Same words, two audiences, two facts.** The dedup revisor
            // refuses such a pair before it ever reaches a model
            // ([`reader_sets_differ`]); the judge meets the pair on a page
            // instead of nominating it, so the gate has to stand here too.
            // Merging them would retire one person's memory and leave the
            // survivor addressing the other's readers — something handed to
            // somebody who was never told it, and unrecoverable once the
            // loser's bytes are off the page.
            // **And the newer one has to actually carry it.** «The winner is
            // the newer copy because it carries whatever the later turn added»
            // holds only while the later turn ADDED: a claim said again in
            // fewer words is a poorer copy of itself, and merging into it takes
            // a figure out of the memory nobody asked to lose. The one way a
            // value may go is into the box it came out of — the words it stands
            // beside — so a newer copy putting a new figure where the old one
            // stood is DISAGREEING, which is a different act and not this one's
            // to refuse. The same mechanism the turn's own reconciler uses, for
            // the same reason (`ingest::vet_supersede`,
            // `compiler::orphaned_values`).
            let orphaned = crate::compiler::orphaned_values(&loser.text, &winner.text);
            if !orphaned.is_empty() {
                tracing::info!(
                    winner = winner.fact_id.as_str(),
                    loser = loser.fact_id.as_str(),
                    lost = ?orphaned,
                    "rem: page judgement would merge into a copy that says less — refused"
                );
                return Ok(VerdictOutcome::Refused("the new says less than the old"));
            }
            if reader_sets_differ(winner, loser) {
                return Ok(VerdictOutcome::Refused(
                    "these two are told to different people, so they are two facts",
                ));
            }
            let op = wal::begin_rem_op(
                pool,
                cycle_id,
                "judgement_duplicate",
                Some(page.wiki_id.as_str()),
                None,
            )
            .await?;
            let hints = dedup::DedupMergeHints {
                jaccard: None,
                source_wiki_id: Some(winner.wiki_id.clone()),
                reason: Some(format!(
                    "rem page judge: the same claim twice on {}",
                    page.source_path
                )),
            };
            let merged = dedup::apply_dedup_merge_direct(
                pool,
                tree,
                Arc::clone(embedder),
                &winner.fact_id,
                &loser.fact_id,
                &hints,
                proposals::recipient_from_fact(&loser.subject_id, loser.sender_id.as_ref()),
            )
            .await;
            match merged {
                Ok(receipt) => {
                    wal::complete_rem_op(pool, op).await?;
                    Ok(VerdictOutcome::Applied(
                        loser.fact_id.as_str().to_owned(),
                        "duplicate_of",
                        format!("merged into {} ({})", winner.fact_id, receipt.proposal_id),
                    ))
                },
                Err(e) => {
                    wal::fail_rem_op(pool, op, &format!("{e}")).await?;
                    Ok(VerdictOutcome::Refused("the merge did not apply"))
                },
            }
        },

        _ => Ok(VerdictOutcome::Refused("no such verdict")),
    }
}

/// The facts a split names, checked against the page it names them on.
///
/// Moving everything is a rename, not a split, and the rung above this one
/// (the page → sub-wiki regrouping) is where a whole page goes.
fn facts_that_move<'a>(
    page: &'a JudgedPage,
    split: &LlmPageSplit,
) -> std::result::Result<Vec<&'a FactIndexRow>, &'static str> {
    let mut moving: Vec<&FactIndexRow> = Vec::with_capacity(split.markers.len());
    for raw in &split.markers {
        let Some(row) = crate::marked_page::marker_number(raw).and_then(|n| page.fact_at(n)) else {
            return Err("no such marker on this page");
        };
        if !moving.iter().any(|f| f.fact_id == row.fact_id) {
            moving.push(row);
        }
    }
    if moving.is_empty() || moving.len() >= page.facts.len() {
        return Err("a split leaves something behind: name the facts that MOVE");
    }
    Ok(moving)
}

/// The page a split opens, from the name the judge gave it.
///
/// The same canonical chokepoint every model-coined page name goes through, so
/// a split cannot coin a second spelling of a concept that already has a page
/// — nor land on one of the engine's own reserved names, which `slugify`
/// leaves untouched.
fn page_the_split_opens(
    split: &LlmPageSplit,
    moving: &[&FactIndexRow],
    source_page_rel: &str,
) -> std::result::Result<String, &'static str> {
    let canonical = split
        .title
        .as_deref()
        .and_then(crate::planner::canonical_page_path)
        .unwrap_or_else(|| default_target_page(&moving[0].text));
    let slug = crate::planner::slugify(canonical.strip_suffix(".md").unwrap_or(&canonical));
    if slug.is_empty() {
        return Err("that name makes no page");
    }
    if crate::wiki::is_reserved_page_stem(&slug) {
        return Err("that name is the engine's own");
    }
    let target = format!("{slug}.md");
    if target == source_page_rel {
        return Err("the new page is the one it would move from");
    }
    Ok(target)
}

/// Carry out the page-level half of one reading: the split, when the judge
/// asked for one.
///
/// **The same writer the promotion pass uses.** A split is a move of a run of
/// facts onto a page of their own, and it lands through
/// [`promote::apply_paragraph_to_file_direct`] with the same born-applied
/// `wiki_promote` receipt it has always had — so the Proposals page goes on
/// telling a reader that the memory split a page, in those words, whichever
/// reading decided it.
///
/// Every guard the promotion pass applies applies here, because a judgement
/// made in one breath with five others is not a reason to write something a
/// separate call would have been refused: a proper non-empty subset, a
/// canonical page name that is not a reserved stem, and the persisted plan
/// re-homed so the next build does not fight the move.
async fn apply_page_split(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &JudgedPage,
    split: &LlmPageSplit,
    cycle_id: &str,
) -> Result<VerdictOutcome> {
    let Some(source_page_rel) = page.page_in_wiki.clone() else {
        return Ok(VerdictOutcome::Refused("this page is not inside a wiki"));
    };
    if !page.heavy_enough_to_split {
        return Ok(VerdictOutcome::Refused(
            "this page was not asked whether it holds two subjects",
        ));
    }
    let moving = match facts_that_move(page, split) {
        Ok(m) => m,
        Err(why) => return Ok(VerdictOutcome::Refused(why)),
    };
    let target = match page_the_split_opens(split, &moving, &source_page_rel) {
        Ok(t) => t,
        Err(why) => return Ok(VerdictOutcome::Refused(why)),
    };
    let target_slug = target.strip_suffix(".md").unwrap_or(&target).to_owned();

    // A run of facts moves as one operation, so its receipt cannot be written
    // once per person the way a closure's can: when the run belongs to more
    // than one of them it is addressed to nobody, because it describes a
    // change to the shape of the memory that concerns all of them.
    let recipient = {
        let mut people = moving
            .iter()
            .map(|f| proposals::recipient_from_fact(&f.subject_id, f.sender_id.as_ref()));
        let first = people.next().flatten();
        people.all(|p| p == first).then_some(first).flatten()
    };
    let hints = ParagraphToFileHints {
        trigger_page_facts: Some(page.facts.len()),
        recall_count_30d: moving.iter().map(|f| f.recall_count_30d).max(),
        reason: Some(format!(
            "rem page judge: {n} of {mass} facts move to {target}",
            n = moving.len(),
            mass = page.facts.len(),
        )),
    };
    let fact_ids: Vec<FactId> = moving.iter().map(|f| f.fact_id.clone()).collect();
    let op = wal::begin_rem_op(
        pool,
        cycle_id,
        "judgement_split",
        Some(page.wiki_id.as_str()),
        None,
    )
    .await?;
    match promote::apply_paragraph_to_file_direct(
        pool,
        tree,
        &page.wiki_id,
        &source_page_rel,
        &fact_ids,
        &target,
        &hints,
        recipient,
    )
    .await
    {
        Ok(receipt) => {
            wal::complete_rem_op(pool, op).await?;
            // Plan-sync seam: re-home the moved facts in the persisted plan so
            // the next build's carry-over does not fight the move. Soft — the
            // move is applied and journaled either way.
            let seed = crate::planner::RehomePageSeed::concept(&target_slug, &page.wiki_id);
            let plan_moves: Vec<(&FactIndexRow, &crate::planner::RehomePageSeed)> =
                moving.iter().map(|f| (*f, &seed)).collect();
            if let Err(e) = crate::planner::rehome_facts_in_persisted_plan(
                tree,
                &plan_moves,
                &[],
                &chrono::Utc::now().to_rfc3339(),
            ) {
                tracing::warn!(error = %e, "page judge: plan re-home failed (the move stands)");
            }
            Ok(VerdictOutcome::Applied(
                page.source_path.clone(),
                "split",
                format!(
                    "{n} facts to {target} ({})",
                    receipt.proposal_id,
                    n = moving.len()
                ),
            ))
        },
        Err(e) => {
            wal::fail_rem_op(pool, op, &format!("{e}")).await?;
            tracing::warn!(error = %e, page = page.source_path, "page judge: split did not apply");
            Ok(VerdictOutcome::Refused("the split did not apply"))
        },
    }
}

/// The destination and the fact one link names, or why the engine drops it.
///
/// Three fences, each dropping a single choice rather than the whole answer —
/// one bad line does not cost the good ones beside it:
///
/// - the destination must be one this page was **offered**, so a name the model
///   invented reaches nothing;
/// - the marker must name a fact of THIS page, because the question is about
///   one fact and an answer that cannot say which fact was not that answer;
/// - two choices naming one destination are one rail, and the second would park
///   nothing while spending a slot of the page's room to say so.
fn accepted_link<'p, 'c>(
    choice: &'c LlmPageLink,
    page: &'p JudgedPage,
    ask: &RailsAsk,
    taken: &mut BTreeSet<&'c str>,
) -> std::result::Result<(&'c str, &'p FactIndexRow), &'static str> {
    let to = choice.to.trim();
    if !ask.offered.contains(to) {
        return Err("a destination this page was not offered");
    }
    let row = crate::marked_page::marker_number(&choice.for_marker)
        .and_then(|n| page.fact_at(n))
        .ok_or("no such marker on this page")?;
    if !taken.insert(to) {
        return Err("this answer names that destination twice");
    }
    Ok((to, row))
}

/// Park the links one reading decided.
///
/// Two fences stand here — the page has to have been ASKED, so a model
/// answering a question its prompt did not carry parks nothing, and the page's
/// remaining room caps how many are parked, so one night can fill a page but
/// not flood it — and the rest are [`accepted_link`]'s, per choice.
///
/// A rail is parked on the plan and becomes a mandatory recommended link at
/// that page's next rewrite, with the same born-applied `rail_add` receipt the
/// rail writer leaves — so a reader of the Proposals page sees one kind of
/// event whichever reading decided it.
#[allow(
    clippy::too_many_arguments,
    reason = "one page's links: what was decided, about which page, by whom, and where both halves of the answer are written down"
)]
async fn apply_page_rails(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &JudgedPage,
    links: &[LlmPageLink],
    cycle_id: &str,
    model_id: &str,
    report: &mut PageJudgementReport,
    outcomes: &mut PageOutcomes,
) -> Result<()> {
    if let Some(ask) = &page.rails
        && links.is_empty()
    {
        // «Nowhere» is an answer, and it is the common one. Remembered here,
        // the rail writer does not carry this page to the same slot tomorrow
        // to be told the same thing ([`rails_memo_subject`]).
        let key = rem_verdicts::key(model_id, &ask.memo_subject);
        rem_verdicts::record_negative(pool, rem_verdicts::kind::RAIL, &key, &page.source_path)
            .await?;
    }
    if links.is_empty() {
        return Ok(());
    }
    // Every refusal is written down where the page's own refusals go, because
    // that is the promise this pass makes: a reading the engine would not carry
    // out is named rather than dropped in silence. It reaches the page's
    // receipt whenever the reading also did something (see
    // `PageOutcomes::worth_a_receipt`), and the night's report either way.
    let refuse = |report: &mut PageJudgementReport,
                  outcomes: &mut PageOutcomes,
                  marker: &str,
                  why: &'static str| {
        tracing::warn!(
            page = page.source_path,
            marker,
            why,
            "page judge: link refused"
        );
        report
            .refused
            .push(format!("{}:rails · {why}", page.source_path));
        outcomes.refused.push(serde_json::json!({
            "marker": marker,
            "verdict": "rail",
            "reason": why,
        }));
    };

    let Some(ask) = &page.rails else {
        refuse(report, outcomes, "(the page)", "not asked of this page");
        return Ok(());
    };
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    for choice in links {
        if taken.len() == ask.budget {
            refuse(
                report,
                outcomes,
                &choice.for_marker,
                "no room left on this page",
            );
            break;
        }
        let (to, row) = match accepted_link(choice, page, ask, &mut taken) {
            Ok(pair) => pair,
            Err(why) => {
                refuse(report, outcomes, &choice.for_marker, why);
                continue;
            },
        };
        // Only added, never swapped: this page was asked at all because it has
        // room, so nothing has to go to make space.
        let Some(outcome) = park_one_rail(
            pool,
            tree,
            cycle_id,
            &ask.slug,
            to,
            Some(row.fact_id.as_str()),
            None,
            &choice.why,
        )
        .await
        else {
            refuse(
                report,
                outcomes,
                &choice.for_marker,
                "the plan would not take that link",
            );
            continue;
        };
        report
            .changed
            .push(format!("{} → {} · rail", ask.slug, outcome.to));
        if let Some(receipt) = outcome.receipt {
            report.receipts.push(receipt);
        }
        outcomes.applied.push(serde_json::json!({
            "marker": choice.for_marker,
            "fact_id": row.fact_id.as_str(),
            "verb": "rail",
            "detail": format!("{} → {}", ask.slug, outcome.to),
        }));
    }
    Ok(())
}

/// One receipt per page, born applied: what the judge changed, and what it
/// asked for that the engine would not do.
async fn emit_page_receipt(
    pool: &SqlitePool,
    page: &JudgedPage,
    depth: JudgementDepth,
    outcomes: &PageOutcomes,
) -> Result<String> {
    let context = serde_json::json!({
        "source_path": page.source_path,
        "wiki_id": page.wiki_id,
        "pass": depth.label(),
        "applied": outcomes.applied,
        "asked": outcomes.asked,
        "refused": outcomes.refused,
    });
    let recipient = page
        .subject
        .as_ref()
        .and_then(|s| proposals::recipient_from_fact(s, page.sender.as_ref()));
    let params = proposals::EmitParams::new(
        proposals::kind::PAGE_JUDGED,
        context.clone(),
        serde_json::json!([]),
    )
    .with_recipient(recipient);
    let emitted = proposals::emit_applied_proposal(pool, params, context, Some("rem")).await?;
    Ok(emitted.proposal_id)
}

// ---------- Cross-wiki refile sweep sub-job ----------

/// Bundled judgment prompt for the cross-wiki refile sweep. Operator
/// override: `<workdir>/prompts/rem-refile.md`.
pub const BUNDLED_REM_REFILE_MD: &str = include_str!("../prompts/rem-refile.md");

/// How much closer-to-foreign-than-home a fact must embed before the
/// cosine pre-filter nominates it. A pure **resource** margin (skip the
/// LLM on facts that sit at least as close to home as to anything
/// foreign), NOT a semantic "belongs elsewhere" gate — the LLM still makes
/// the verdict. The margin keeps a fact home unless a foreign wiki is
/// materially more similar, so a fact merely adjacent to two subjects is
/// never nominated.
const REFILE_COSINE_MARGIN: f32 = 0.05;

/// Per-wiki view that scores and presents refile candidates: the
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
    /// The foreign wikis this fact may be offered, best first, each carrying
    /// the reason it is there ([`ranked_foreign`]).
    foreign: Vec<ForeignOffer<'a>>,
}

/// The LLM's verdict for one refile candidate (shared by the refile
/// sweep and the recall-repair proposal — same closed shape).
#[derive(Debug, Default, serde::Deserialize)]
struct RefileDecision {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    dest_wiki_id: Option<String>,
    /// The page inside `dest_wiki_id` the fact lands on. It MUST be one the
    /// destination already has: the compilation plan keys pages by a bare
    /// slug across the whole forest, so a NEW name in a foreign wiki can
    /// collide with a same-named page homed elsewhere and attach the fact to
    /// the wrong wiki's node. Choosing among pages that already exist keeps
    /// the key the one the plan already holds.
    #[serde(default)]
    dest_page: Option<String>,
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
/// [`REFILE_COSINE_MARGIN`].
///
/// **What the day wrote comes first**, then newest-first inside each band, and
/// the cap then cuts the tail. The perimeter is an ordering and never a
/// filter: a fact the day did not touch is still nominated, just behind the
/// ones that landed since the last cycle — because *is this fact on the right
/// page?* is a question about a placement somebody just made, and asking it
/// first about material nothing has touched in months spends the night's
/// budget on the part of the corpus least likely to have moved.
fn refile_cases<'a>(
    views: &'a [RefileWikiView<'a>],
    day: &day::DayPerimeter,
    turn_of: &HashMap<String, String>,
    policy: &RemPolicy,
) -> Vec<RefileCase<'a>> {
    let mut cases: Vec<(bool, &str, RefileCase<'a>)> = Vec::new();
    for home in views {
        for fact in &home.facts {
            // The reserved rules page is the rules pipeline's perimeter, not
            // the refile's: a per-user behaviour rule embeds close to its
            // *user's* wiki by nature (it names how the agent behaves with
            // them), so it is a natural false nominee — and a confirmed move
            // would eject it from the behaviour-rules channel, which reads
            // `@rules.md` in the agent's own wiki (the refile twin of the
            // compiler-door skip in `planner::gather_standard_facts`), and the
            // signposts page is fenced the same way. Channel facts still count
            // in the similarity pools above/below; they are only never
            // nominated as the fact to move.
            if wiki::is_channel_page(&fact.source_path) {
                continue;
            }
            let foreign = ranked_foreign(views, home, fact, turn_of, true);
            if foreign.is_empty() {
                continue;
            }
            cases.push((
                day.touched_fact(fact.fact_id.as_str()),
                fact.created_at.as_str(),
                RefileCase {
                    fact,
                    home,
                    foreign,
                },
            ));
        }
    }
    // What the day wrote first, then newest-first inside each band; the cap
    // then favours what just landed.
    cases.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    cases.truncate(policy.refile_sweep_cap);
    cases.into_iter().map(|(_, _, c)| c).collect()
}

/// Why a foreign wiki is in front of the refile judge.
///
/// The same idea as [`crate::candidates::CandidateSource`], one level up: a
/// list built on similarity alone offers only wikis that already sound like
/// the fact, and a fact about Bob sitting in Alice's wiki does not have to
/// sound like Bob's other facts to belong with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForeignReason {
    /// It is this fact's subject's or sender's own wiki.
    People,
    /// It holds a fact extracted from the same conversational turn.
    Turn,
    /// Its facts sit closest to this one.
    Near,
}

impl ForeignReason {
    const fn tag(self) -> &'static str {
        match self {
            Self::People => "same-people",
            Self::Turn => "same-turn",
            Self::Near => "near",
        }
    }
}

/// A foreign wiki as the judge sees it: the wiki, and why it is being offered.
struct ForeignOffer<'a> {
    view: &'a RefileWikiView<'a>,
    reason: ForeignReason,
}

/// Whether `view` **is** the identity wiki of `fact`'s subject or sender.
///
/// The sharp form of the question, and deliberately not "does this wiki hold
/// any fact about them": a wiki holds facts about many people — a behaviour
/// rule about Bob lives in the agent's wiki by design — so the loose form
/// admits nearly every wiki and says nothing. An identity wiki's id **is** its
/// principal's ([`crate::wiki::IDENTITY_WIKI_TYPE`]), so this is an equality,
/// and what it answers is the case this sweep exists for: a fact about Bob
/// filed somewhere that is not Bob's.
fn wiki_is_the_subjects_own(view: &RefileWikiView<'_>, fact: &FactIndexRow) -> bool {
    let wiki = view.d.meta.wiki_id.as_str();
    let names = |p: &crate::types::Principal| match p {
        crate::types::Principal::User(id) | crate::types::Principal::Group(id) => id == wiki,
    };
    names(&fact.subject_id) || fact.sender_id.as_ref().is_some_and(names)
}

/// Whether `view` holds a fact from one of `turns`.
fn wiki_shares_turns(
    view: &RefileWikiView<'_>,
    turns: &HashSet<String>,
    turn_of: &HashMap<String, String>,
) -> bool {
    view.facts.iter().any(|f| {
        turn_of
            .get(f.fact_id.as_str())
            .is_some_and(|t| turns.contains(t))
    })
}

/// The foreign wikis this fact may be offered, best first, each carrying the
/// reason it is there.
///
/// Ranked by best cosine, and **admitted by three different questions**. With
/// `margin` the cosine pre-filter applies to the `near` question only (a
/// foreign wiki must beat home by [`REFILE_COSINE_MARGIN`] — the
/// self-nomination valve); the other two admit a wiki whatever the vectors
/// say, because they answer something the vectors do not:
///
/// - **same people** — the wiki **is** this fact's subject's or sender's own
///   ([`wiki_is_the_subjects_own`]). A fact about Bob filed in Alice's wiki is
///   the whole case this sweep exists for, and it does not have to *sound*
///   like Bob's other facts to belong with them;
/// - **same turn** — the wiki holds a fact from the same conversation. Two
///   facts of one turn sit at 0.256 textual similarity, so similarity will
///   never put them together.
///
/// Without `margin` every foreign wiki ranks: the reviewer-fed bridge already
/// nominated the fact, so the pre-filter has nothing left to decide.
fn ranked_foreign<'a>(
    views: &'a [RefileWikiView<'a>],
    home: &RefileWikiView<'a>,
    fact: &FactIndexRow,
    turn_of: &HashMap<String, String>,
    margin: bool,
) -> Vec<ForeignOffer<'a>> {
    let home_best = best_cosine_to_wiki(fact, home);
    let turns: HashSet<String> = turn_of
        .get(fact.fact_id.as_str())
        .cloned()
        .into_iter()
        .collect();

    let mut foreign: Vec<(f32, ForeignOffer<'a>)> = views
        .iter()
        .filter(|v| v.d.meta.wiki_id != home.d.meta.wiki_id)
        .filter_map(|v| {
            let score = best_cosine_to_wiki(fact, v);
            // Order matters only for the label: a wiki that answers two of
            // the questions is offered once, named by the sharpest.
            let reason = if wiki_is_the_subjects_own(v, fact) {
                ForeignReason::People
            } else if wiki_shares_turns(v, &turns, turn_of) {
                ForeignReason::Turn
            } else if !margin || score >= home_best + REFILE_COSINE_MARGIN {
                ForeignReason::Near
            } else {
                return None;
            };
            Some((score, ForeignOffer { view: v, reason }))
        })
        .collect();
    foreign.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    foreign.into_iter().map(|(_, o)| o).collect()
}

/// `fact_id → origin_message_hash`, for every live fact that came through the
/// capture buffer with a turn recorded on it.
///
/// One query for the sweep, joined in memory afterwards. Never an error: a map
/// that could not be read is empty, and the `same-turn` question then admits
/// nobody — which is the behaviour the sweep had before it could ask.
async fn turns_by_fact(pool: &SqlitePool) -> HashMap<String, String> {
    match sqlx::query_as::<_, (String, String)>(
        "SELECT c.capture_id, c.origin_message_hash
           FROM capture_buffer c
          WHERE c.origin_message_hash IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().collect(),
        Err(e) => {
            tracing::warn!(error = %e, "refile: turns unread — the sweep asks two questions instead of three");
            HashMap::new()
        },
    }
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

/// The pages one wiki already holds, read off its directory.
///
/// The same rule as [`refile_wiki_pages`] for a caller that has the wiki but
/// not its facts: reserved names and engine files are excluded, so what comes
/// back is only what a foreign fact may be aimed at.
fn wiki_pages_on_disk(d: &wiki::DiscoveredWiki) -> std::collections::BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(&d.abs_dir) else {
        return std::collections::BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            let path = std::path::Path::new(&name);
            (path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
                && !name.starts_with('_')
                && !wiki::names_reserved_page(path))
            .then_some(name)
        })
        .collect()
}

/// The pages a wiki already holds, derived from the facts on them — no
/// second read, and no page the sweep may not aim at.
///
/// Reserved names are filtered out: an identity card carries one subject and
/// a channel page is written by its own code path, so neither is a landing
/// place for somebody else's fact.
fn refile_wiki_pages(view: &RefileWikiView<'_>) -> std::collections::BTreeSet<String> {
    view.facts
        .iter()
        .filter_map(|f| wiki_relative_page(view.d, &f.source_path))
        .filter(|p| !wiki::names_reserved_page(std::path::Path::new(p)))
        .collect()
}

/// One candidate wiki as the judge sees it: **why it is here**, the wiki line,
/// and the pages it already has, which are the only landing places it may name.
///
/// The reason is on the line because the three questions that admit a wiki
/// mean different things. `near` says its facts sound like this one, which is
/// weak evidence on its own. `same-people` says it holds facts about this
/// fact's subject — and a fact filed away from its own subject is the case
/// this sweep exists for.
fn refile_candidate_block(offer: &ForeignOffer<'_>) -> String {
    let pages = refile_wiki_pages(offer.view);
    let pages = if pages.is_empty() {
        "(none yet — this wiki cannot take a fact until it has a page)".to_owned()
    } else {
        pages.into_iter().collect::<Vec<_>>().join(", ")
    };
    format!(
        "[{}] {}\n    pages: {pages}",
        offer.reason.tag(),
        refile_wiki_line(offer.view)
    )
}

/// The cross-wiki refile sweep — the LLM-decided refile of a single
/// misfiled fact into a different existing wiki.
///
/// A deterministic cosine pre-filter nominates facts that embed
/// materially closer to a foreign wiki than to home (a **resource** cap — it
/// only nominates, never decides); the revisor LLM (`llms.revisor` — the low
/// binary-classifier confirmer tier) decides whether (and where) each really
/// belongs. A confirmed move
/// lands **act-first** via [`promote::apply_fact_refile_direct`] with the
/// same born-applied receipt the other REM act-first sub-jobs write. Smart
/// wikis are skipped as **both** source and destination: the smart-family
/// is the consumer's, and refiling into/out of it would corrupt the
/// ownership boundary (smart rows carry projected wiki-level ACL).
async fn run_refile_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    day: &day::DayPerimeter,
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
    // Which turn each fact came out of — the question similarity cannot ask.
    let turn_of = turns_by_fact(pool).await;

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
        let foreign = ranked_foreign(&views, home, fact, &turn_of, false);
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
    for case in refile_cases(&views, day, &turn_of, policy) {
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
        .map(|o| refile_candidate_block(o))
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
        .map(|o| o.view)
        .find(|v| v.d.meta.wiki_id.as_str() == dest_wiki_id)
    else {
        tracing::warn!(
            dest = dest_wiki_id,
            "rem refile: confirmer named a non-candidate dest — skipped"
        );
        return Ok(None);
    };
    // Destination page: one the destination wiki ALREADY has. The
    // compilation plan keys pages by a bare slug across the whole forest, so
    // a NEW name in a foreign wiki can collide with a same-named page homed
    // elsewhere — the rehome would attach the fact to the WRONG wiki's node
    // and the next compile would strand `wiki_id != source_path` (a
    // cross-wiki leak). A page the destination already holds is already in
    // the plan under the right wiki, so naming it cannot mint a second key.
    let dest_pages = refile_wiki_pages(dest_view);
    let Some(dest_page) = decision
        .dest_page
        .as_deref()
        .map(str::trim)
        .filter(|p| dest_pages.contains(*p))
    else {
        tracing::warn!(
            dest = dest_wiki_id,
            page = ?decision.dest_page,
            "rem refile: confirmer named no page the destination already has — skipped"
        );
        return Ok(None);
    };
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
        proposals::recipient_from_fact(&case.fact.subject_id, case.fact.sender_id.as_ref());
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
    will_be_read: &BTreeSet<&str>,
) -> Result<ContradictionSweepReport> {
    let mut report = ContradictionSweepReport::default();
    if policy.contradiction_sweep_cap == 0 {
        return Ok(report);
    }
    let since = (now - policy.closure_sweep_window).to_rfc3339();
    let mut cases: Vec<(FactIndexRow, Vec<FactIndexRow>)> = Vec::new();
    // One scope per standard wiki: a contradiction fells the satellites that
    // live with it.
    let scopes = consolidation_scopes(tree, smart_wiki_index)?;
    let agent_family = agent_wikis(&scopes);
    for scope in &scopes {
        let seeds = fact_index::find_recently_contradicted(pool, &scope.wiki_id, &since).await?;
        if seeds.is_empty() {
            continue;
        }
        // Still in force: an open horizon, OR a horizon that has not passed
        // yet.
        //
        // A dated satellite is the whole point of this sub-job — the cancelled
        // trip's itinerary days, which `ingest.md` tells the classifier to give
        // a concrete `valid_to` ("a dated commitment or deadline ends at its
        // own time"). Requiring `valid_to IS NULL` made this pass and the
        // due-soon slot (`find_due_between`, which requires `valid_to IS NOT
        // NULL`) disjoint by construction: the one class of fact that keeps
        // firing after its event is cancelled was the one class this sweep
        // could never nominate. Already-expired rows stay out — closing what
        // has already lapsed spends a confirmer call to change nothing.
        let open_rows: Vec<FactIndexRow> = fact_index::find_active_in_wiki(pool, &scope.wiki_id)
            .await?
            .into_iter()
            .filter(|r| {
                r.valid_to.as_deref().is_none_or(|t| {
                    chrono::DateTime::parse_from_rfc3339(t).is_ok_and(|ts| ts.to_utc() > now)
                })
            })
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
            //    supersede, tombstone, or its subject's explicit closure, never
            //    as collateral of a neighbouring contradiction. Same fence the
            //    dedup/refile sweeps already honour
            //    ([`crate::wiki::is_channel_page`]).
            // 3. An identity-core fact (a role / relationship — `bio` +
            //    `salience=high`) is sticky: it changes only on the subject's
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
            // A fact and every neighbour it is weighed against, all on a page
            // the reading will open: that reading answers `contradicted` with
            // the whole page in front of it
            // ([`all_on_one_page_the_reading_will_open`]).
            if all_on_one_page_the_reading_will_open(&seed, &candidates, will_be_read) {
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
        let agent = agent_family
            .get(seed.wiki_id.as_str())
            .copied()
            .unwrap_or(false);
        match judge_contradiction_case(pool, tree, llm, cycle_id, &seed, &candidates, agent).await {
            Ok(Some((receipt_ids, closed))) => {
                report.receipts.extend(receipt_ids);
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
    agent_family: bool,
) -> Result<Option<(Vec<String>, Vec<String>)>> {
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
                fact_began(c),
                fact_claim(&c.text)
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
            (
                "subject_note",
                if agent_family {
                    AGENT_CONTRADICTION_NOTE
                } else {
                    ""
                },
            ),
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
    // The invalidation instant: when the seed STOPPED BEING TRUE — not when it
    // was once due to end, and not when the engine noticed.
    //
    // A `valid_to` already BEHIND us is the answer:
    // `fact_index::mark_superseded` closes it at the instant the replacement
    // began, which is the instant the seed fell and therefore the instant its
    // satellites fell with it. `superseded_at` dates the sweep that read it,
    // which on a backlog replay is months away from anything that happened.
    //
    // A `valid_to` still AHEAD of us is a different thing and unusable here:
    // `mark_superseded` writes `valid_to = COALESCE(valid_to, ?)`, so a seed
    // that already carried its own future expiry KEEPS it. Anchoring a
    // satellite to that horizon stamps the satellite with a future `valid_to`,
    // and `find_due_between` matches on exactly that — which would push the
    // just-cancelled satellite INTO the due-soon slot the closure exists to get
    // it out of.
    // The pass's own «now» is the memory's, not the wall's: a night catching
    // up on a backlog dates what it closes by the story it is reading
    // ([`fact_index::memory_now`]). On a live installation the two are the
    // same instant to within minutes.
    let now = fact_index::memory_now(pool).await;
    let seed_closed_at = seed
        .valid_to
        .clone()
        .filter(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok_and(|ts| ts.to_utc() <= now))
        .or_else(|| seed.superseded_at.clone())
        .unwrap_or_else(|| fact_index::bound_from_instant(now));
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
            .and_then(|b| fact_index::canonical_bound(b, fact_index::DayEdge::End))
            .unwrap_or_else(|| seed_closed_at.clone());
        // A satellite falls with the seed, so it inherits the seed's
        // superseding fact as its successor (None when the seed was closed
        // without one — the pointer stays empty rather than guessing).
        //
        // Where the seed's date cannot reach — a satellite that BEGAN after
        // the seed was closed, which is how the cluster sweep wrote eleven
        // facts that ended before they started — the honest answer is this
        // sweep's own instant: the satellite was in force until the pass
        // decided it had fallen, and when it stopped being true is precisely
        // what nobody here knows. It is the same last resort `seed_closed_at`
        // already falls back to ([`fact_index::end_not_before_start`]).
        let Some(prev) = fact_index::close_validity(
            pool,
            &target.fact_id,
            &valid_to,
            fact_index::decay::CONTRADICTED,
            seed.superseded_by.as_ref(),
            now,
        )
        .await?
        else {
            continue;
        };
        // As above: the seed's instant is what the satellite is dated by only
        // where the satellite can carry it
        // ([`fact_index::ClosedValidity::written_valid_to`]).
        let valid_to = prev.written_valid_to.clone();
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

    // The seed's own text rides in the gesture, so it goes only to the
    // person the seed belongs to; the rest are told that a fact of theirs
    // fell, and not what it fell with.
    let gesture = format!(
        "REM contradiction sweep — fell with: {}",
        fact_preview(&seed.text)
    );
    let seed_recipient = proposals::recipient_from_fact(&seed.subject_id, seed.sender_id.as_ref());
    let mut receipts = Vec::new();
    for (recipient, closed) in proposals::group_by_recipient(&applied, |c| {
        // A closed target that is not among the candidates belongs to
        // nobody we can name: falling back to the seed's owner would
        // address somebody else's receipt to them. Unreachable today, and
        // the same answer the ingest side gives.
        candidates
            .iter()
            .find(|k| k.fact_id == c.fact_id)
            .and_then(|row| proposals::recipient_from_fact(&row.subject_id, row.sender_id.as_ref()))
    }) {
        let said = (recipient == seed_recipient).then_some(gesture.as_str());
        let receipt = match promote::emit_validity_close_receipt(
            pool,
            &closed,
            said,
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
        receipts.push(receipt.proposal_id);
    }
    wal::complete_rem_op(pool, op_id).await?;
    let closed = applied
        .iter()
        .map(|c| c.fact_id.as_str().to_owned())
        .collect();
    Ok(Some((receipts, closed)))
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

    let Some((dest, dest_page, reason)) = decision else {
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
    let recipient = proposals::recipient_from_fact(&fact.subject_id, fact.sender_id.as_ref());
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
    let scratch_dest_page = dest_page.clone();
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
                &scratch_dest_page,
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
    // refile sweep (a born-applied receipt), and onto the same page: the
    // gate proved the flip against THAT
    // destination, so committing to any other page would ship a move the
    // replay never judged.
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
        &dest_page,
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
) -> Option<(&'a wiki::DiscoveredWiki, String, Option<String>)> {
    let candidates_text = candidates
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let pages = wiki_pages_on_disk(d);
            let pages = if pages.is_empty() {
                "(none yet — this wiki cannot take a fact until it has a page)".to_owned()
            } else {
                pages.into_iter().collect::<Vec<_>>().join(", ")
            };
            format!(
                "{}. {} · {} — {}\n    pages: {pages}",
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
    // The page must be one the destination already has: a new name in a
    // foreign wiki mints a second plan key under the bare-slug keyspace and
    // strands `wiki_id != source_path`.
    let dest_page = decision
        .dest_page
        .as_deref()
        .map(str::trim)
        .filter(|p| wiki_pages_on_disk(dest).contains(*p))?
        .to_owned();
    Some((dest, dest_page, decision.reason))
}

/// The body of the recall-tuning notice: which fact keeps going missing,
/// where it lives, how often, and what the gate said about the repair that
/// was tried.
///
/// Written once and read twice, because the notice has two homes and they
/// must say the same thing: the event a consumer running as the operator can
/// drain, and the Proposals row the operator meets in the dashboard
/// ([`file_recall_tuning_notice`]).
///
/// It carries no sample of what the person ASKED. The query that missed is
/// their own sentence, restated — the most private thing in the whole miss —
/// and a notice is a payload that travels: it leaves over MCP, sits in a
/// consumer's context and is handed to whatever model that consumer runs. The
/// operator needs it and has it, in `recall_log`, where it is stored with the
/// miss and read under the operator's own eyes.
fn recall_tuning_payload(
    fact: &FactIndexRow,
    fact_id: &str,
    miss_count: i64,
    gate_note: Option<&str>,
) -> serde_json::Value {
    json!({
        "fact_id": fact_id,
        "wiki_id": fact.wiki_id,
        "source_path": fact.source_path,
        "miss_count": miss_count,
        "gate": gate_note,
        "hint": "recurring recall miss with no provable local repair — a recall-tuning \
                 lever (fact topics, recall knobs, navigator prompt) needs the operator; \
                 never auto-applied",
    })
}

/// Give the recall-tuning notice its home in the dashboard: one Proposals row
/// carrying the same body the event carries.
///
/// The event on its own reaches nobody who is not holding a token: a notice
/// addressed to nobody comes out of `events_poll` only for the operator's own
/// one, so an operator who works the dashboard — which is the ordinary way —
/// would meet this nowhere. The Proposals page is where the engine already
/// puts what it wants a human to look at.
///
/// **Born applied and addressed to nobody.** There is nothing to apply: the
/// levers this points at are never moved automatically, so the row is evidence
/// for a person to read, and it reaches its final state the moment it is
/// written. Unaddressed because it is about the engine's own recall rather
/// than about anybody's facts.
///
/// Best-effort: a failure here is logged and the cycle carries on. The notice
/// is a prompt to a human, and losing one is not worth failing a REM pass for.
async fn file_recall_tuning_notice(pool: &SqlitePool, body: &serde_json::Value) {
    let params = crate::proposals::EmitParams::new(
        crate::proposals::kind::RECALL_TUNING,
        body.clone(),
        json!([]),
    );
    if let Err(e) =
        crate::proposals::emit_applied_proposal(pool, params, body.clone(), Some("rem")).await
    {
        tracing::warn!(error = %e, "rem: recall-tuning notice not recorded on the proposals page");
    }
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
        let body = recall_tuning_payload(fact, &miss.fact_id, count, gate_note);
        events::insert_event(
            pool,
            EventKind::RecallTuningProposed,
            Some(&fact.wiki_id),
            Some(miss.fact_id.as_str()),
            &body,
        )
        .await
        .map_err(|e| e.to_string())?;
        file_recall_tuning_notice(pool, &body).await;
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
/// The match pins one exact shape — ` ([[wiki/page]])` appended to a
/// non-empty claim — and nothing else: the target must be a plain
/// `wiki/page` pointer (a `/`, no
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
/// Mechanical repair of one defect shape in the stored rows, not a semantic
/// gate. A claim whose body ends in the dossier backlink (` ([[wiki/page]])`)
/// floods the document page with inbound links, feeds link noise to embeddings
/// and dedup, and freezes prose the Cronista cannot restyle. Provenance rides
/// `authored_refs`, so nothing writes that shape; this sweep converges the
/// rows that carry it. Per flagged fact: move the
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
/// rows are ALL tombstoned or superseded.
///
/// The compiler's orphan sweep (`sweep_orphan_page_files`, every
/// compile) already drops a plan-absent file with **no** non-tombstoned
/// rows; it keeps a file while a superseded row still points at it. The
/// file is a husk (a supersede's leftover obituary page, a placeholder
/// whose only fact fell): this sweep removes it and settles the retired
/// rows' stale offsets. Inbound links degrade to literal
/// text at render (the link grammar's dead-rail posture — never a
/// broken link) and the compile feed's dead-ref vetting keeps prose
/// clean, so no link rewriter is needed.
///
/// Deterministic, no LLM: a structural GC behind DB-first guards, not a
/// semantic judgment (each fact was closed by its own judged path).
/// `@rules.md` and `_`-prefixed files never qualify; smart
/// wikis are skipped (consumer-authored files are never REM's to
/// delete); **no plan on disk → no-op** (a fresh workdir's pages are
/// unplanned, not husks). Bounded by `policy.husk_gc_cap` per cycle,
/// oldest wikis/pages first by path order so a backlog drains
/// deterministically.
async fn run_husk_gc(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
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
                || name == wiki::RULES_FILENAME
                || pages.is_some_and(|p| p.contains(name))
                || !entry.path().is_file()
            {
                continue;
            }
            let source_path = wiki::workdir_relative_source_path(tree.workdir(), &entry.path());
            report.pages_examined += 1;
            match fact_index::count_husk_blocking_rows(pool, &source_path).await {
                Ok(0) => {
                    removable.push((wiki_id.to_owned(), source_path, entry.path()));
                },
                Ok(_) => {}, // an active row keeps the file
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
            "rem husk-gc: husk page removed (plan-absent, no active row left)"
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
/// model (`llms.revisor`), which rewrites each relative phrase against
/// **the instant that fact's sentence existed** ([`fact_began`]) — a
/// backfilled "oggi" belongs to the day it was uttered, not to the day its
/// row was inserted. An applied rewrite re-embeds the text and updates
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

    // One call PER WIKI, not one for the whole forest. A rewrite is fact
    // prose, so it needs the wiki's language directive — and a directive
    // only means something over a batch that belongs to one wiki. The
    // selection and the cap above stay global, so the cycle still spends
    // at most `date_normalize_cap` facts; only how they are dealt out
    // changes. Deterministic: BTreeMap orders the wikis, and the
    // oldest-first sort survives inside each one.
    let mut by_wiki: BTreeMap<&str, Vec<&FactIndexRow>> = BTreeMap::new();
    for row in &flagged {
        by_wiki.entry(row.wiki_id.as_str()).or_default().push(row);
    }
    let mut rewrites: Vec<DateRewrite> = Vec::new();
    for (wiki, rows) in by_wiki {
        let facts_text = rows
            .iter()
            .enumerate()
            .map(|(i, f)| {
                // When "today" was said, which is what a relative phrase
                // resolves against.
                format!(
                    "{}. {} · {} · {}",
                    i + 1,
                    f.fact_id.as_str(),
                    fact_began(f),
                    f.text.replace('\n', " ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let language_directive = match crate::types::WikiId::parse(wiki) {
            Ok(id) => crate::locale::memory_directive_for_wiki(pool, tree, &id).await,
            Err(_) => crate::locale::render_memory_language_directive(None),
        };
        let prompt = prompts::render(
            "rem-dates",
            tree.workdir(),
            BUNDLED_REM_DATES_MD,
            &[
                ("locale", language_directive.as_str()),
                ("facts", facts_text.as_str()),
            ],
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
                // One wiki's transport failure must not sink the whole
                // cycle's normalisation — the others still drain.
                report.errors.push(format!("normalizer LLM ({wiki}): {e}"));
                continue;
            },
        };
        let parsed = first_json_object(&resp.text)
            .and_then(|v| serde_json::from_value::<DateRewrites>(v).ok());
        let Some(decision) = parsed else {
            report
                .errors
                .push(format!("normalizer: unparseable LLM answer ({wiki})"));
            continue;
        };
        rewrites.extend(decision.rewrites);
    }
    if rewrites.is_empty() {
        return Ok(report);
    }
    let decision = DateRewrites { rewrites };

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
        // media link.
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
/// Runs once per REM cycle, after the briefing emitter.
/// Two passes (see the module docstring of
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

// ---------- Briefing-processor non-smart ----------

/// Drain pending `wiki_briefing_items` rows whose `wiki_id` is a
/// non-smart wiki and whose `ts` is older than the configured
/// grace period.
///
/// On smart wikis the inbox is drained by the smart consumer at
/// `smart_bootstrap` via `mark_processed` on the next `wiki_admin_push`.
/// Narrative families (every non-smart wiki: `wiki-user`, `wiki-group`, and
/// topic wikis) have no smart consumer, so REM
/// fills the gap by calling the **same** core function
/// ([`briefing_processor::process_briefing_item`]) the dashboard's
/// "Mark as read" button calls synchronously — one branch, two callers,
/// no drift.
///
/// Policy: **mark-passive** (see [`briefing_processor`] module doc).
/// The grace period guards against draining a comment the operator is
/// still editing through the dashboard — "Mark as read" bypasses the
/// grace, the cycle does not.
///
/// Per-row outcomes from `process_briefing_item`:
///
/// - `Processed` → counted in `items_processed`.
/// - `AlreadyProcessed` → counted in `items_already_processed` (a
///   concurrent pass drained the row between the candidate scan and the
///   per-row call).
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
    // (`smart_wiki_index`, the per-cycle `_meta.md` snapshot). A non-smart
    // wiki is a standard wiki, and its comments get **action-taking**: they
    // are interpreted into fact ops, batched per wiki (read together,
    // applied together).
    let mut standard_by_wiki: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut mark_passive: Vec<i64> = Vec::new();
    for (bi_id, wiki_id) in candidates {
        // Smart family: the smart consumer owns the drain, REM must not
        // touch it. Unknown wikis (deleted between the snapshot and now) are
        // absent from the index and fall through to the per-row handler which
        // surfaces them as `WikiNotFound`.
        match smart_wiki_index.get(&wiki_id) {
            // Smart wiki: the smart consumer owns the drain — skip (no-op).
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
            report.comment_ops_refused.extend(applied.refused);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{self, CaptureRequest};
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;
    use crate::types::{FactId, Principal, WikiId};

    /// Bundle the one LLM the existing tests want (the revisor;
    /// auto-promote and auto-apply default disabled).
    fn test_llms(revisor: &FakeLlmBackend) -> RemLlms<'_> {
        RemLlms {
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
    }

    fn fake_embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.1, 0.2, 0.3, 0.4],
        ))
    }

    /// Plant a `pending` `dedup_merge` row whose `timeout_at` sits
    /// `timeout_offset_secs` from now (negative = already overdue).
    /// Raw SQL: the engine has no emitter for the pending lifecycle, and
    /// the sweeps are what this file tests.
    async fn plant_pending_dedup_merge(
        pool: &SqlitePool,
        winner: &str,
        loser: &str,
        timeout_offset_secs: i64,
    ) -> String {
        let proposal_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let context = json!({ "winner_fact_id": winner, "loser_fact_id": loser });
        let questions = json!([{
            "id": "confirm",
            "text": "Merge these two near-duplicate facts?",
            "options": [{"id": "merge", "label": "Merge", "recommended": true}],
        }]);
        sqlx::query(
            "INSERT INTO structure_proposals
                 (proposal_id, kind, context, questions, proposed_at, timeout_at, status)
             VALUES (?, 'dedup_merge', ?, ?, ?, ?, 'pending')",
        )
        .bind(&proposal_id)
        .bind(context.to_string())
        .bind(questions.to_string())
        .bind(now.to_rfc3339())
        .bind((now + chrono::Duration::seconds(timeout_offset_secs)).to_rfc3339())
        .execute(pool)
        .await
        .expect("plant pending dedup_merge");
        proposal_id
    }

    async fn plant_fact(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        body: &str,
        subject: &str,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("preferenze.md")),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
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

    /// [`plant_fact`] with the validity window spelled out — the closure
    /// passes reason about WHEN a fact held, so their tests must set it.
    async fn plant_fact_with_window(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        body: &str,
        subject: &str,
        valid_from: Option<String>,
        valid_to: Option<String>,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("preferenze.md")),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: Vec::new(),
            dedup_threshold: Some(0.999),
            valid_from,
            valid_to,
            style: None,
            page_description: None,
            salience: None,
        };
        capture::wiki_capture(tree, pool, fake_embedder(), req)
            .await
            .expect("plant")
            .fact_id
    }

    /// [`plant_fact`] with the ACL axes spelled out — the audience gate is
    /// the only guard that reads them, so its tests must set them.
    async fn plant_fact_with_acl(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        body: &str,
        subject: &str,
        allow: Vec<Principal>,
        sender: Option<Principal>,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("preferenze.md")),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
            allow,
            sender,
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
    /// `wiki_sections` (no capture, no ACL, no lifecycle), so the briefing
    /// dispatcher reads these rows when it scans a smart wiki.
    ///
    /// Returns the section's stable `"<source_path>#<ord>"` handle.
    async fn plant_section(pool: &SqlitePool, wiki: &str, body: &str) -> String {
        let source_path = format!("wikis/{wiki}/preferenze.md");
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
        subject: &str,
        embedding: Vec<f32>,
    ) -> FactId {
        plant_page_fact_with_embedding(tree, pool, wiki, "preferenze.md", body, subject, embedding)
            .await
    }

    /// [`plant_fact_with_embedding`] on a caller-chosen page (e.g. the
    /// reserved `@rules.md`, for the behaviour-rules channel guards).
    async fn plant_page_fact_with_embedding(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        body: &str,
        subject: &str,
        embedding: Vec<f32>,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
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

    // ---------- auto-promote gate: already_promoted_for scoping ----------

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
    /// page-promotion receipts (`paragraph_to_file`), scoped to the same
    /// `(source_wiki_id, source_page)`.
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
            "preferenze.md",
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
            already_promoted_for(&pool, &f, "hermes1", "preferenze.md")
                .await
                .unwrap(),
            "a genuine paragraph_to_file receipt must veto its own source page"
        );

        // An expired receipt must not veto; a pending one (in flight) must.
        let g = FactId::parse("018f1234-5678-7abc-9def-000000000002").unwrap();
        insert_wiki_promote_proposal(
            &pool,
            "p-sub-expired",
            "paragraph_to_file",
            "hermes1",
            "trio.md",
            &[g.as_str()],
            "expired",
        )
        .await;
        assert!(
            !already_promoted_for(&pool, &g, "hermes1", "trio.md")
                .await
                .unwrap(),
            "an expired receipt must not veto"
        );
        insert_wiki_promote_proposal(
            &pool,
            "p-sub-pending",
            "paragraph_to_file",
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        // The born-applied receipt records what happened + the context shape.
        let proposal_id = &report.revisor.applied[0];
        let (kind, status, context): (String, String, String) = sqlx::query_as(
            "SELECT kind, status, context FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "dedup_merge");
        assert_eq!(status, "applied", "born-applied receipt, no pending stage");
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(
            ctx.get("winner_fact_id").and_then(|v| v.as_str()),
            Some(new_id.as_str()),
        );
        assert_eq!(
            ctx.get("loser_fact_id").and_then(|v| v.as_str()),
            Some(old_id.as_str()),
        );
        // The event stream says nothing: a merge the revisor decided is
        // housekeeping, and the receipt is where it is written down.
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM wiki_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0, "a nightly merge notices nobody");
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");

        let first = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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

        let second = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let llms = test_llms(&rev_llm);

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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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

    /// Plant `n` distinct short facts on `wiki`'s `preferenze.md` so the page
    /// accumulates mass. Bodies are distinct topics so the jaccard
    /// pre-pass never flags them as dedup siblings.
    async fn plant_distinct(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        n: usize,
        subject: &str,
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
            out.push(plant_fact(tree, pool, wiki, t, subject).await);
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
        subject: &str,
    ) -> FactId {
        plant_fact_with_embedder(tree, pool, fake_embedder(), wiki, page, body, subject).await
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
        subject: &str,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
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
                    primary_facts: facts,
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
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
            authored_rails: Vec::new(),
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
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &index,
            &BTreeSet::new(),
            Utc::now(),
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

        // Born-applied receipt: a record of the merge.
        let status: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.applied[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "applied");

        // The nightly cycle reports nothing.
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_events WHERE kind = 'structure_applied' AND payload LIKE '%page_merge%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "the nightly cycle notices nobody");

        // Persisted plan re-homed: husk gone, survivor holds all the facts
        // and is parked for the next compile's weave.
        let plan = crate::planner::load_previous_plan(&tree).unwrap().unwrap();
        assert!(!plan.pages.contains_key("viaggi_parigi"));
        assert_eq!(plan.pages["viaggi"].primary_facts.len(), 3);
        assert!(plan.force_dirty.contains(&"viaggi".to_owned()));

        // The pair is now judged, so it is never re-judged.
        assert!(
            merge_already_judged(&pool, "alice", "viaggi.md", "alice", "viaggi_parigi.md")
                .await
                .unwrap()
        );
        // …and only that pair: the veto is about two pages, not about every
        // page whose name contains one of theirs.
        assert!(
            !merge_already_judged(&pool, "alice", "viaggi.md", "alice", "parigi.md")
                .await
                .unwrap(),
            "a page whose name is a substring of the judged one is not judged"
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
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &index,
            &BTreeSet::new(),
            Utc::now(),
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

    /// A confirmer that has stopped answering costs the pair, not the
    /// night: the merge sub-job returns its partial report and the pages
    /// stay as they were, unjudged, for the next cycle to ask again.
    #[tokio::test]
    async fn page_merge_survives_a_failing_confirmer() {
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

        let llm = FlakyLlm {
            fail_first: std::sync::atomic::AtomicUsize::new(usize::MAX),
            answer: String::new(),
        };
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_page_merge(
            &pool,
            &tree,
            &llm,
            "cycle-down",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &index,
            &BTreeSet::new(),
            Utc::now(),
        )
        .await
        .expect("a dead confirmer costs the sub-job, never the cycle");

        assert_eq!(report.candidates_examined, 1, "the pair was nominated");
        assert_eq!(report.errors.len(), 1, "the skip is recorded: {report:?}");
        assert_eq!(report.candidates_confirmed, 0);
        assert!(report.applied.is_empty());
        assert!(tree.wikis_dir().join("alice/viaggi.md").exists());
        assert!(tree.wikis_dir().join("alice/viaggi_lavoro.md").exists());
        // Unjudged: the pair comes back tomorrow.
        assert!(
            !merge_already_judged(&pool, "alice", "viaggi.md", "alice", "viaggi_lavoro.md")
                .await
                .unwrap()
        );
        drop(dir);
    }

    /// A fact-bearing concept leaf for the merge-nomination fixtures.
    fn kin_leaf(slug: &str, wiki: &str, n_facts: usize) -> PagePlan {
        let facts = (0..n_facts)
            .map(|i| crate::planner::FactForPage {
                topics: Vec::new(),
                subject_external: None,
                authored_refs: Vec::new(),
                fact_id: FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{i:02x}"))
                    .unwrap(),
                text: format!("fact {i}"),
                fact_type: None,
                subject: "user:alice".parse().unwrap(),
                allow: Vec::new(),
                sender: None,
                source_wiki_id: wiki.to_owned(),
                valid_from: None,
                valid_to: None,
                decay_reason: None,
                successor_fact_id: None,
                target_page: None,
                style: None,
                salience: None,
            })
            .collect();
        PagePlan {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: String::new(),
            style: None,
            primary_facts: facts,
            outgoing_links: Vec::new(),
            pending_links: Vec::new(),
            wiki_id: wiki.to_owned(),
            page_path: format!("{slug}.md"),
        }
    }

    #[test]
    fn a_page_this_cycle_split_off_is_not_a_merge_candidate() {
        // The splitter takes a sub-topic out of a heavy page and names the new
        // page after it, so the pair it leaves behind is kin by construction —
        // the strongest signal the merger has. Left alone, the merger confirms
        // "same concept" (it is) and puts back what was just taken apart, two
        // seconds later. Worse, the split receipt survives, so once the facts
        // are home the anti-double-promotion veto reads the page as already
        // promoted and never offers it again.
        let leaf = kin_leaf;
        let mut pages = std::collections::BTreeMap::new();
        pages.insert("dossier".to_owned(), leaf("dossier", "alice", 3));
        pages.insert(
            "dossier_esami".to_owned(),
            leaf("dossier_esami", "alice", 2),
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
            authored_rails: Vec::new(),
        };
        let consolidating = BTreeSet::from(["alice".to_owned()]);
        let day = day::DayPerimeter::default();

        let open = merge_candidates(&plan, &[], &consolidating, &day, &BTreeSet::new());
        assert!(
            open.iter()
                .any(|(a, b, _)| a == "dossier" && b == "dossier_esami"),
            "without the fence the kin pair nominates: {open:?}"
        );

        let fenced = merge_candidates(
            &plan,
            &[],
            &consolidating,
            &day,
            &BTreeSet::from(["dossier_esami".to_owned()]),
        );
        assert!(
            fenced.is_empty(),
            "a page this cycle split off is not offered back to the merger: {fenced:?}"
        );
    }

    #[test]
    fn merge_candidates_nominate_same_wiki_kin_leaves_only() {
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
        // Kin names in two wikis, however alike the ids → never nominated.
        // A page belongs to one wiki, and consolidation happens inside it;
        // gathering one subject's pages into one wiki is the grouping pass's
        // job, and this pass sees the result, not the scatter.
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
            authored_rails: Vec::new(),
        };
        let consolidating: BTreeSet<String> = ["alice", "bob", "famiglia", "famiglia-bruno"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let got = merge_candidates(
            &plan,
            &[],
            &consolidating,
            &day::DayPerimeter::default(),
            &BTreeSet::new(),
        );
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
            !pairs.contains(&("dossier", "dossier_bruno")),
            "kin names in two wikis do not nominate, however alike the ids: {pairs:?}"
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
        // No cap here: the whole list comes back and the caller spends its
        // budget on the pairs that reach a judgement, so a handful of
        // already-vetoed pairs cannot consume the night's spend without a
        // single call being made.
        let all = merge_candidates(
            &plan,
            &[],
            &consolidating,
            &day::DayPerimeter::default(),
            &BTreeSet::new(),
        );
        let mass = |slug: &str| plan.pages[slug].primary_facts.len();
        let heaviest_pair_mass = mass(&all[0].0) + mass(&all[0].1);
        assert!(
            all.iter()
                .all(|(a, b, _)| mass(a) + mass(b) <= heaviest_pair_mass),
            "the heaviest kin pair leads: {all:?}"
        );
    }

    /// **A page carrying one fact is a merge candidate like any other**, and
    /// that is the net under the closing pass.
    ///
    /// The closing pass may open a page for a single claim rather than leave
    /// it waiting another day (founder, 2026-08-22: *«la pagina risultante con
    /// una sola frase poi crescerà oppure sarà rivista le notti
    /// successive»*). The second half of that ruling is this sweep: if mass
    /// were a floor here, every thin page the closing pass opened would be
    /// invisible to the one thing that can fold it into a better home.
    #[test]
    fn a_one_fact_page_is_a_merge_candidate_like_any_other() {
        let mut pages = std::collections::BTreeMap::new();
        pages.insert("nuoto".to_owned(), kin_leaf("nuoto", "alice", 1));
        pages.insert(
            "nuoto_martedi".to_owned(),
            kin_leaf("nuoto_martedi", "alice", 1),
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let consolidating = BTreeSet::from(["alice".to_owned()]);

        let pairs = merge_candidates(
            &plan,
            &[],
            &consolidating,
            &day::DayPerimeter::default(),
            &BTreeSet::new(),
        );
        assert!(
            pairs
                .iter()
                .any(|(a, b, _)| a == "nuoto" && b == "nuoto_martedi"),
            "two one-fact kin pages are a pair: {pairs:?}"
        );
    }

    /// The corpus both rail-detector tests read: `diario` points at `fisco`,
    /// the other two pages point nowhere, and the walk opened
    /// `documenti`+`fisco` and `diario`+`fisco` three times each.
    async fn co_open_corpus() -> (
        crate::test_db::TestWorkdir,
        SqlitePool,
        WikiTree,
        CompilationPlan,
    ) {
        let (wd, pool, tree) = crate::test_db::TestWorkdir::with_db_and_tree().await;
        let wid = crate::types::WikiId::parse("alice").expect("id");
        crate::wiki::create_identity_wiki(&tree, &wid, "alice", crate::wiki::IdentityKind::User)
            .expect("wiki");
        let handle = tree.locate(&wid).expect("handle");
        // `diario` carries a written link to `fisco`; the others carry none.
        // A link counts only when it is on the page — the plan may recommend
        // one the writing model never wove in, and the reader cannot follow a
        // recommendation.
        handle
            .write_page(
                std::path::Path::new("diario.md"),
                "# Diario\n\nvedi [[alice/fisco]].\n",
            )
            .expect("page");
        for page in ["documenti.md", "fisco.md"] {
            handle
                .write_page(std::path::Path::new(page), "# p\n\nprose\n")
                .expect("page");
        }

        let mut pages = BTreeMap::new();
        for (slug, file) in [
            ("documenti", "documenti.md"),
            ("fisco", "fisco.md"),
            ("diario", "diario.md"),
        ] {
            pages.insert(
                slug.to_owned(),
                crate::planner::PagePlan {
                    slug: slug.to_owned(),
                    title: slug.to_owned(),
                    description: String::new(),
                    style: None,
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: "alice".to_owned(),
                    page_path: file.to_owned(),
                },
            );
        }
        // The plan ALSO recommends documenti->fisco. It must not count: the
        // link was never written, so no reader can travel it. This is the
        // inversion that matters — trusting the plan hid 33 % of the real
        // gaps on the live corpus.
        let mut link_graph = BTreeMap::new();
        link_graph.insert("documenti".to_owned(), vec!["fisco".to_owned()]);
        link_graph.insert("fisco".to_owned(), vec!["documenti".to_owned()]);
        let plan = crate::planner::CompilationPlan {
            pages,
            link_graph,
            merged_pages: Vec::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        // Three turns opened documenti+fisco; three opened diario+fisco.
        for _ in 0..3 {
            crate::recall_log::record_turn(
                &pool,
                "alice",
                &chrono::Utc::now().to_rfc3339(),
                &[],
                &[
                    "wikis/alice/documenti.md".to_owned(),
                    "wikis/alice/fisco.md".to_owned(),
                ],
                &[],
            )
            .await
            .expect("log");
            crate::recall_log::record_turn(
                &pool,
                "alice",
                &chrono::Utc::now().to_rfc3339(),
                &[],
                &[
                    "wikis/alice/diario.md".to_owned(),
                    "wikis/alice/fisco.md".to_owned(),
                ],
                &[],
            )
            .await
            .expect("log");
        }
        (wd, pool, tree, plan)
    }

    /// Pages opened together enough times, with no rail leading from one to
    /// the other, are nominated — strongest evidence first, one entry per
    /// direction that is missing.
    ///
    /// **The one-way pair is the case that matters.** `diario` points at
    /// `fisco`, so a reader on `diario` is fine and `diario` has no gap. A
    /// reader on `fisco` has nowhere to go, and that gap is exactly as real as
    /// one on a pair linked neither way — reading the pair as "joined" would
    /// hide it, which is the flattering direction this detector exists not to
    /// look in.
    #[tokio::test]
    async fn co_opened_pages_are_nominated_for_each_direction_nobody_can_walk() {
        let (_wd, pool, tree, plan) = co_open_corpus().await;
        let rails = detect_missing_rails(&pool, &tree, &plan, 3, 10)
            .await
            .expect("detect");
        let got: Vec<(&str, &str)> = rails
            .iter()
            .map(|r| (r.from_slug.as_str(), r.to_slug.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                // `documenti` and `fisco` are linked neither way: a gap from
                // each side. `fisco` is stranded beside `diario` too, and its
                // two gaps sort together, destination first.
                ("documenti", "fisco"),
                ("fisco", "diario"),
                ("fisco", "documenti"),
            ],
            "and nothing for `diario`, which points at `fisco` already and owes \
             nothing more: {rails:?}"
        );
        assert!(rails.iter().all(|r| r.co_opens == 3), "{rails:?}");
    }

    /// Under the co-open floor nothing is nominated at all: the detector's
    /// whole claim is that a reader went there repeatedly.
    #[tokio::test]
    async fn a_pair_opened_under_the_floor_is_not_nominated() {
        let (_wd, pool, tree, plan) = co_open_corpus().await;
        assert!(
            detect_missing_rails(&pool, &tree, &plan, 4, 10)
                .await
                .expect("detect")
                .is_empty()
        );
    }

    /// The mass floor is about how a page is READ, not how big it is.
    #[test]
    fn a_list_is_never_split_by_mass_and_technical_prose_takes_twice_the_room() {
        let p = RemPolicy::default();
        assert_eq!(
            mass_floor_for_style(Some(crate::wiki::PageStyle::Lista), &p),
            None,
            "a lista is consulted, and half a list answers nothing"
        );
        assert_eq!(
            mass_floor_for_style(Some(crate::wiki::PageStyle::ProsaTecnica), &p),
            Some(p.auto_promote_min_page_facts_technical)
        );
        assert_eq!(
            mass_floor_for_style(Some(crate::wiki::PageStyle::Prosa), &p),
            Some(p.auto_promote_min_page_facts)
        );
        // Absent or drifted → the prose floor. Drifting toward "may be split"
        // is safer than toward "never".
        assert_eq!(
            mass_floor_for_style(None, &p),
            Some(p.auto_promote_min_page_facts)
        );
        // There is no fourth style to pass here: `PageStyle` has three values
        // and nothing else parses into one (2026-08-19), so `None` above is
        // the whole of "something else".
        assert!(
            p.auto_promote_min_page_facts_technical > p.auto_promote_min_page_facts,
            "scanning tolerates more mass than following a thread"
        );
    }

    #[tokio::test]
    async fn auto_promote_is_noop_without_rem_promotions_llm() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Mass is irrelevant — the sub-job short-circuits before walking
        // when no rem_promotions LLM is wired.
        plant_distinct(&tree, &pool, "alice", 3, "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&rev_llm),
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
    fn split_llms<'a>(rev: &'a FakeLlmBackend, promote: &'a FakeLlmBackend) -> RemLlms<'a> {
        RemLlms {
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
            &split_llms(&rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();

        // One page examined whole, one split applied.
        assert_eq!(report.auto_promote.candidates_examined, 1);
        assert_eq!(report.auto_promote.candidates_promoted, 1);
        assert_eq!(report.auto_promote.applied.len(), 1);
        let proposal_id = &report.auto_promote.applied[0];
        // Act-first: the receipt is born `applied` — there is no `pending`
        // stage and no approval step.
        let (kind, status, context): (String, String, String) = sqlx::query_as(
            "SELECT kind, status, context FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "wiki_promote");
        assert_eq!(status, "applied");
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "paragraph_to_file");
        assert_eq!(ctx["source_wiki_id"], "alice");
        // The stored source_page is wiki-relative, not
        // `wikis/alice/preferenze.md`.
        assert_eq!(ctx["source_page"], "preferenze.md");
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

        // Nobody is told: reorganising itself is the memory's own business.
        let notices: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(notices, 0, "a split notices nobody");
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
            &split_llms(&rev_llm, &promote_llm),
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

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"split\": true, \"fact_ids\": [\"whatever\"], \"target_page\": \"x.md\"}",
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&rev_llm, &promote_llm),
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

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new("rp", "{\"split\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&rev_llm, &promote_llm),
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

    /// A model that has stopped answering costs the auto-promote sub-job
    /// and nothing behind it: the failures land in the report, the pages
    /// stay nominable, and `run_cycle` still returns — so `dream::run_full`
    /// reaches the compile and drains the night's captures.
    #[tokio::test]
    async fn auto_promote_survives_a_backend_that_keeps_failing() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Six pages over the mass floor: more split candidates than the
        // sub-job will spend calls on before it stops.
        for page in [
            "orto.md",
            "potatura.md",
            "compost.md",
            "semina.md",
            "innesti.md",
            "serra.md",
        ] {
            plant_on_page(&tree, &pool, "alice", page, 3, "alice").await;
        }
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let llm = FlakyLlm {
            fail_first: std::sync::atomic::AtomicUsize::new(usize::MAX),
            answer: String::new(),
        };
        let report = run_auto_promote(
            &pool,
            &tree,
            Some(&llm),
            "cycle-down",
            &day::DayPerimeter::default(),
            &mass_policy(),
            &load_smart_wiki_index(&tree).expect("index"),
            &BTreeSet::new(),
        )
        .await
        .expect("a dead backend costs the sub-job, never the cycle");

        assert_eq!(
            report.errors.len(),
            LLM_FAILURE_ABORT,
            "it stops at the abort threshold with a partial report: {report:?}"
        );
        assert!(report.applied.is_empty(), "nothing split: {report:?}");
        // Nothing was memoised, so every page is nominable again tomorrow.
        let settled: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rem_verdicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(settled, 0);
        drop(dir);
    }

    /// The grouping pass reads the same rule: one failed call costs its
    /// wiki, the sub-job returns, and the split loop behind it runs on.
    #[tokio::test]
    async fn page_grouping_survives_a_failing_model() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let llm = FlakyLlm {
            fail_first: std::sync::atomic::AtomicUsize::new(usize::MAX),
            answer: String::new(),
        };
        let report = run_auto_promote(
            &pool,
            &tree,
            Some(&llm),
            "cycle-flaky",
            &day::DayPerimeter::default(),
            &grouping_policy(),
            &load_smart_wiki_index(&tree).expect("index"),
            &BTreeSet::new(),
        )
        .await
        .expect("a fumbled grouping call is a soft error, never a cycle abort");

        assert_eq!(report.grouping_wikis_examined, 1);
        assert_eq!(report.errors.len(), 1, "one call, one skip: {report:?}");
        assert_eq!(report.grouping_groups_applied, 0);
        assert!(report.applied.is_empty());
        // The pages stay where they were, so the next cycle asks again.
        for page in ["orto.md", "potatura.md", "compost.md"] {
            assert!(tree.wikis_dir().join("alice").join(page).exists());
        }
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
            &split_llms(&rev_llm, &promote_llm),
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

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            format!(
                "{{\"split\": true, \"fact_ids\": [\"{}\"], \"target_page\": \"acme-corp.md\"}}",
                facts[0].as_str()
            ),
        );
        let llms = split_llms(&rev_llm, &promote_llm);
        let r1 = run_cycle(&pool, &tree, fake_embedder(), &llms, &mass_policy())
            .await
            .unwrap();
        assert_eq!(r1.auto_promote.applied.len(), 1);
        // Cycle 2: the page dropped under the floor (2 facts) and the
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

    // ---------- rail writer ----------

    /// **The REM decides a link, and the decision becomes a mandatory rail.**
    ///
    /// The founder's sentence — *«i link che il navigatore segue alla fine li
    /// ha decisi il REM»* — was false while nothing in the cycle wrote one.
    /// This is the pass that makes it true: it looks at a page from outside,
    /// picks one destination out of the four candidate sources, parks it on
    /// the plan, and leaves a receipt.
    #[tokio::test]
    async fn the_rail_writer_parks_the_link_it_chose_and_leaves_a_receipt() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        for slug in ["cucina", "intolleranze", "auto"] {
            pages.insert(slug.to_owned(), kin_leaf(slug, "alice", 2));
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 6,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let llm = FakeLlmBackend::new(
            "pro",
            "{\"links\":[{\"link\":\"intolleranze\",\"for_fact\":1,\"instead_of\":null,\
             \"why\":\"a reader of this fact needs it and would never search for it\"}]}",
        );
        let policy = RemPolicy {
            rail_writer_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &policy,
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");

        assert_eq!(report.nominated, 3, "three pages carry no link at all");
        assert_eq!(report.judged, 1, "and the cap judges one of them");
        assert_eq!(report.written.len(), 1, "{report:?}");
        assert_eq!(report.written[0].1, "intolleranze");
        assert_eq!(report.receipts.len(), 1);

        // The decision is on the plan, waiting for the next rewrite to write
        // it into prose.
        let saved = crate::planner::load_previous_plan(&tree)
            .expect("load")
            .expect("plan");
        assert!(
            saved
                .authored_rails
                .contains(&(report.written[0].0.clone(), "intolleranze".to_owned())),
            "{:?}",
            saved.authored_rails
        );

        // And the owner can read what the engine decided.
        let (kind, status): (String, String) =
            sqlx::query_as("SELECT kind, status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt");
        assert_eq!(kind, "rail_add");
        assert_eq!(status, "applied", "act-first: born applied");

        // The model was shown the candidates with the source that offered
        // each — the whole point of the fourth source is that the model can
        // tell a `far` page from a `near` one.
        let shown = llm.last_prompt().expect("the model was asked");
        assert!(shown.contains("CANDIDATE DESTINATIONS"), "{shown}");
        assert!(shown.contains("] intolleranze:"), "{shown}");
        drop(dir);
    }

    /// One page, several facts, several links — and each one names its fact.
    ///
    /// A single destination per page would be an answer to a question about
    /// the page. The question is asked of one fact at a time, so it has as
    /// many answers as there are facts that need one, and the page's own
    /// remaining room is what bounds them.
    #[tokio::test]
    async fn a_page_gets_one_rail_per_fact_that_needs_one() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        for slug in ["cucina", "intolleranze", "auto"] {
            pages.insert(slug.to_owned(), kin_leaf(slug, "alice", 2));
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 6,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        // Two good answers, then two the fences drop: one naming a page that
        // was never offered, one naming no fact at all.
        let llm = FakeLlmBackend::new(
            "pro",
            "{\"links\":[\
               {\"link\":\"intolleranze\",\"for_fact\":1,\"why\":\"decides what may be eaten\"},\
               {\"link\":\"cucina\",\"for_fact\":2,\"why\":\"decides when it can be cooked\"},\
               {\"link\":\"inventata\",\"for_fact\":1,\"why\":\"a page nobody offered\"},\
               {\"link\":\"spesa\",\"why\":\"and this one names no fact\"}]}",
        );
        let policy = RemPolicy {
            rail_writer_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &policy,
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");

        // Every page carries no link, so the cap judges the first by slug.
        assert_eq!(report.judged, 1, "one page, one judgement");
        assert!(
            report.written.iter().all(|(from, _)| from == "auto"),
            "one page answered, and both its links came from it: {report:?}"
        );
        let mut written: Vec<&str> = report.written.iter().map(|(_, to)| to.as_str()).collect();
        written.sort_unstable();
        assert_eq!(
            written,
            vec!["cucina", "intolleranze"],
            "both answers that clear the fences are parked: {report:?}"
        );
        assert_eq!(report.receipts.len(), 2, "one receipt each");

        // The facts are numbered in the page block, because `for_fact` is an
        // index into exactly that list.
        let shown = llm.last_prompt().expect("the model was asked");
        assert!(shown.contains("1. "), "the facts are numbered: {shown}");
        drop(dir);
    }

    /// The page's own remaining room bounds the night, so one pass can fill a
    /// page and never flood it: a page carrying five of its six budgeted links
    /// has room for one more, whatever the model returns.
    #[tokio::test]
    async fn the_page_budget_bounds_what_one_night_may_add() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        for slug in ["cucina", "intolleranze", "auto"] {
            pages.insert(slug.to_owned(), kin_leaf(slug, "alice", 2));
        }
        let mut link_graph = BTreeMap::new();
        link_graph.insert(
            "cucina".to_owned(),
            (0..PAGE_RAIL_BUDGET - 1)
                .map(|i| format!("v{i}"))
                .collect::<Vec<_>>(),
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 6,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let llm = FakeLlmBackend::new(
            "pro",
            "{\"links\":[\
               {\"link\":\"intolleranze\",\"for_fact\":1,\"why\":\"first\"},\
               {\"link\":\"auto\",\"for_fact\":2,\"why\":\"second\"}]}",
        );
        // `cucina` carries the most links, so it is nominated last; the other
        // two carry none. Judge all three and count what `cucina` was allowed.
        let policy = RemPolicy {
            rail_writer_cap: 3,
            ..RemPolicy::default()
        };
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &policy,
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");

        let from_cucina = report
            .written
            .iter()
            .filter(|(from, _)| from == "cucina")
            .count();
        assert_eq!(
            from_cucina, 1,
            "one seat left, one rail parked, whatever the model returned: {report:?}"
        );
        drop(dir);
    }

    /// `none` is a real answer, and the common one: a link nobody needs costs
    /// a clause of prose on every future rewrite and buys nothing.
    #[tokio::test]
    async fn the_rail_writer_writes_nothing_when_the_model_says_none() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        for slug in ["cucina", "auto"] {
            pages.insert(slug.to_owned(), kin_leaf(slug, "alice", 2));
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 4,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let llm = FakeLlmBackend::new("pro", "{\"links\":[]}");
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");
        assert!(report.written.is_empty(), "{report:?}");
        assert!(report.receipts.is_empty());
        assert!(
            crate::planner::load_previous_plan(&tree)
                .expect("load")
                .expect("plan")
                .authored_rails
                .is_empty()
        );
        drop(dir);
    }

    /// **A page whose prose carries a link is not this pass's to change.**
    ///
    /// The swap half of the founder's ruling is a swap of *its own* rails: a
    /// link the Cronista wrote, or one an admin typed into the dashboard's raw
    /// editor, belongs to whoever wrote it. A model naming one in `instead_of` is ignored, and
    /// the new rail is added beside it.
    #[tokio::test]
    async fn the_rail_writer_never_removes_a_link_the_prose_carries() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        for slug in ["cucina", "intolleranze", "spesa"] {
            pages.insert(slug.to_owned(), kin_leaf(slug, "alice", 2));
        }
        // `cucina` already links `spesa`, and the prose is where that came
        // from — the plan carries no authored rail for it. The other two are
        // pushed to the budget so `cucina` is the only page nominated.
        let mut link_graph = BTreeMap::new();
        link_graph.insert("cucina".to_owned(), vec!["spesa".to_owned()]);
        for slug in ["spesa", "intolleranze"] {
            link_graph.insert(
                slug.to_owned(),
                (0..PAGE_RAIL_BUDGET).map(|i| format!("v{i}")).collect(),
            );
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 6,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let llm = FakeLlmBackend::new(
            "pro",
            "{\"links\":[{\"link\":\"intolleranze\",\"for_fact\":1,\"instead_of\":\"spesa\",\
             \"why\":\"better\"}]}",
        );
        let policy = RemPolicy {
            rail_writer_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &policy,
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");
        assert!(
            report.replaced.is_empty(),
            "a link the prose carries is not this pass's to remove: {report:?}"
        );
        let saved = crate::planner::load_previous_plan(&tree)
            .expect("load")
            .expect("plan");
        assert_eq!(saved.authored_rails.len(), 1, "{:?}", saved.authored_rails);

        // And the model was told which of its links it may touch.
        let shown = llm.last_prompt().expect("asked");
        assert!(
            shown.contains("not yours to remove"),
            "the list says who wrote each link: {shown}"
        );
        drop(dir);
    }

    /// A page already at the budget is not nominated: the pass adds links, it
    /// does not fill pages with addresses.
    #[tokio::test]
    async fn a_page_at_the_rail_budget_is_not_nominated() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut pages = std::collections::BTreeMap::new();
        pages.insert("piena".to_owned(), kin_leaf("piena", "alice", 2));
        let mut link_graph = BTreeMap::new();
        link_graph.insert(
            "piena".to_owned(),
            (0..PAGE_RAIL_BUDGET).map(|i| format!("v{i}")).collect(),
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 2,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let llm = FakeLlmBackend::new(
            "pro",
            "{\"links\":[{\"link\":\"qualcosa\",\"for_fact\":1}]}",
        );
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&llm),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");
        assert_eq!(report.nominated, 0, "{report:?}");
        assert!(llm.last_prompt().is_none(), "and no call was bought");
        drop(dir);
    }

    /// **A pair the day touched leads**, whatever put it on the list.
    ///
    /// The caller spends a fixed budget of judgements. Two pages that both
    /// stopped moving months ago will still be there tomorrow; a page opened
    /// this morning for a single claim is the one that most needs somewhere
    /// better to be, and the closing pass opens exactly those.
    #[test]
    fn a_merge_pair_the_day_touched_is_judged_first() {
        let mut pages = std::collections::BTreeMap::new();
        // The heavier pair — which the mass ranking puts first — is old.
        pages.insert("viaggi".to_owned(), kin_leaf("viaggi", "alice", 9));
        pages.insert(
            "viaggi_parigi".to_owned(),
            kin_leaf("viaggi_parigi", "alice", 9),
        );
        // The thin pair is what the day opened.
        pages.insert("nuoto".to_owned(), kin_leaf("nuoto", "alice", 1));
        pages.insert(
            "nuoto_martedi".to_owned(),
            kin_leaf("nuoto_martedi", "alice", 1),
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let consolidating = BTreeSet::from(["alice".to_owned()]);

        let quiet = merge_candidates(
            &plan,
            &[],
            &consolidating,
            &day::DayPerimeter::default(),
            &BTreeSet::new(),
        );
        assert_eq!(
            (quiet[0].0.as_str(), quiet[0].1.as_str()),
            ("viaggi", "viaggi_parigi"),
            "with no perimeter the heaviest pair leads: {quiet:?}"
        );

        let day = day::DayPerimeter {
            since: Some(Utc::now()),
            pages_born: std::iter::once("nuoto_martedi".to_owned()).collect(),
            ..day::DayPerimeter::default()
        };
        let today = merge_candidates(&plan, &[], &consolidating, &day, &BTreeSet::new());
        assert_eq!(
            (today[0].0.as_str(), today[0].1.as_str()),
            ("nuoto", "nuoto_martedi"),
            "the pair the day opened leads instead: {today:?}"
        );
        assert_eq!(
            today.len(),
            quiet.len(),
            "and nothing is dropped — the perimeter orders, it never filters"
        );
    }

    /// **The model is told which metre it is measured on.**
    ///
    /// The floor that let a page reach the split prompt is not one number —
    /// a `prosa` page loses its thread early, a `prosa-tecnica` page is
    /// scanned and tolerates more, a `lista` is a set and is never split for
    /// size. Without the sentence the model is asked whether a page "grew
    /// disproportionately" while the only scale it has is the fact count, so
    /// it answers the same way for a bullet list and for a narrative.
    #[test]
    fn the_split_model_is_told_the_floor_its_page_was_measured_on() {
        use crate::wiki::PageStyle;
        let p = RemPolicy::default();

        let prosa = shape_directive(Some(PageStyle::Prosa), &p);
        assert!(prosa.contains("narrative thread"), "{prosa}");
        assert!(
            prosa.contains(&p.auto_promote_min_page_facts.to_string()),
            "it names the floor it passed: {prosa}"
        );

        let tecnica = shape_directive(Some(PageStyle::ProsaTecnica), &p);
        assert!(
            tecnica.contains(&p.auto_promote_min_page_facts_technical.to_string()),
            "and it is a different floor: {tecnica}"
        );
        assert_ne!(prosa, tecnica);

        let lista = shape_directive(Some(PageStyle::Lista), &p);
        assert!(lista.contains("never split for"), "{lista}");
        assert!(
            !lista.contains(&p.auto_promote_min_page_facts.to_string()),
            "a list has no floor to name: {lista}"
        );

        // An unreadable testata falls to the prose floor, the same way
        // `mass_floor_for_style` does — one metre, named once.
        assert_eq!(shape_directive(None, &p), prosa);
    }

    /// A channel page is never split by mass, whatever its size.
    ///
    /// The floor is style and mass, and neither can see what a page is FOR. A
    /// diary over the floor would otherwise reach the model that names the
    /// facts to move out, and a confirmed split would take them onto a page of
    /// their own — where the channel, which keys on the path, never looks.
    #[tokio::test]
    async fn auto_promote_never_splits_a_channel_page() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Well over any floor, on the diary and on an ordinary page alike.
        plant_on_page(&tree, &pool, "alice", "@projects_diary.md", 12, "alice").await;
        plant_on_page(&tree, &pool, "alice", "cucina.md", 12, "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The scorer says "split" to anything it is shown, so whatever reaches
        // it gets split — which is exactly what makes the absence visible.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"split\":true,\"fact_ids\":[\"n1\",\"n2\"],\"target_page\":\"nuova.md\"}",
        );
        let policy = RemPolicy::default();
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &grouping_llms(&rev_llm, &promote_llm),
            &policy,
        )
        .await
        .expect("cycle");

        // One page reached the scorer, and it is the ordinary one: without
        // the gate both would, since both sit well over the floor. Asserting
        // on the candidate count rather than on receipt ids, which are opaque
        // uuids and would make the check pass vacuously.
        //
        // Deliberately NOT asserting the diary's fact count: the dedup sweep
        // is allowed to fold two diary lines into each other (its fence
        // forbids crossing the channel boundary, not pairing inside it), so
        // that number moves for reasons unrelated to the split.
        assert_eq!(
            report.auto_promote.candidates_examined, 1,
            "only the ordinary page may reach the scorer: {:?}",
            report.auto_promote
        );
        drop(dir);
    }

    /// **The identity card is never split, whatever its mass.**
    ///
    /// The negative half is what this denies, and it is what shipped until
    /// 2026-08-22: the card is prose, so it hit the ordinary 8-fact floor
    /// like any topic page and reached the scorer. A confirmed split moves
    /// facts off it onto a page the walk may never reach — and the card is
    /// the one page recall serves WHOLE into every turn, so the turn quietly
    /// stops being told them.
    #[tokio::test]
    async fn auto_promote_never_splits_the_identity_card() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Well over any floor, on the card and on an ordinary page alike.
        plant_on_page(&tree, &pool, "alice", "@profile.md", 12, "alice").await;
        plant_on_page(&tree, &pool, "alice", "cucina.md", 12, "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The scorer splits anything it is shown, so whatever reaches it is
        // split — which is what makes the absence visible.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"split\":true,\"fact_ids\":[\"n1\",\"n2\"],\"target_page\":\"nuova.md\"}",
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &grouping_llms(&rev_llm, &promote_llm),
            &RemPolicy::default(),
        )
        .await
        .expect("cycle");

        assert_eq!(
            report.auto_promote.candidates_examined, 1,
            "only the ordinary page may reach the scorer — without the gate \
             both would, since both sit well over the floor: {:?}",
            report.auto_promote
        );
        drop(dir);
    }

    // ---------- page-group → wiki regrouping ----------

    /// Plant `n` distinct facts on a specific page so the regrouping pass
    /// has real topic pages to work with. Bodies are namespaced by page so two pages
    /// never collide on the dedup threshold.
    /// A page of the same wiki on another subject, so the group under test is
    /// not the whole wiki — which is the one shape the nominator drops (see
    /// [`already_fills_a_wiki`]).
    async fn plant_off_topic_page(tree: &WikiTree, pool: &SqlitePool, wiki: &str, subject: &str) {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("bollette.md")),
            body: "la bolletta della luce arriva ogni due mesi".to_owned(),
            subject: Principal::User(subject.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: vec!["bollette".to_owned(), "casa".to_owned()],
            dedup_threshold: Some(0.999),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        capture::wiki_capture(tree, pool, fake_embedder(), req)
            .await
            .expect("plant off-topic");
    }

    async fn plant_on_page(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        n: usize,
        subject: &str,
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
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse(wiki).unwrap(),
                page: Some(PathBuf::from(page)),
                body: format!("{page}: {t}"),
                subject: Principal::User(subject.to_owned()),
                allow: Vec::new(),
                sender: None,
                fact_type: None,
                // Every real fact carries its two words, and the grouping
                // nominates from them — a fixture with none ties nothing
                // together and the pass rightly has nothing to ask about.
                topics: vec![
                    page.trim_end_matches(".md").to_owned(),
                    "giardino".to_owned(),
                ],
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

    fn grouping_llms<'a>(rev: &'a FakeLlmBackend, promote: &'a FakeLlmBackend) -> RemLlms<'a> {
        RemLlms {
            revisor: rev,
            auto_promote: Some(promote),
            apply: None,
            comment_applier: None,
            cronista: None,
            navigator: None,
        }
    }

    /// The registry entries an earlier build left behind, pinning each page to
    /// the wiki it sits in. This is the state a live memory is always in — a
    /// page exists because some build registered it — and it is what the next
    /// full rebuild reads to decide where the page goes.
    fn pin_concepts_to_wiki(tree: &WikiTree, concepts: &[&str], wiki_id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let mut reg = crate::planner::load_concept_registry(tree, &now).unwrap();
        for concept in concepts {
            reg.entries.insert(
                (*concept).to_owned(),
                crate::planner::ConceptRegistryEntry {
                    slug: (*concept).to_owned(),
                    title: (*concept).to_owned(),
                    description: String::new(),
                    style: None,
                    wiki_id: wiki_id.to_owned(),
                    created_at: now.clone(),
                },
            );
        }
        crate::planner::save_concept_registry(tree, &reg).unwrap();
    }

    /// The promotion has to survive the compile that follows it in the same
    /// night.
    ///
    /// Its sibling above proves the structural half with no Cronista wired, so
    /// the cycle ends before anything rewrites a page. A real night has one:
    /// the compile runs after the promotion, rebuilds each dirty page from the
    /// plan and repoints its rows at what it wrote. If the plan did not follow
    /// the pages into the new wiki, that compile puts them back — the wiki
    /// keeps the files nothing points at, the pages reappear where they came
    /// from, and a reader opening the newborn wiki is served regions whose
    /// facts live somewhere else, which renders as a page of `[redacted]`.
    #[tokio::test]
    async fn a_founded_wiki_survives_the_compile_that_follows_it() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        pin_concepts_to_wiki(&tree, &["orto", "potatura", "compost"], "alice");

        // A claim still waiting is what makes the closing pass do its work
        // instead of returning on an empty queue — and its work is a FULL
        // rebuild of the plan, which is the pass this test exists to put the
        // promotion in front of.
        crate::capture_buffer::buffer_capture(
            &pool,
            crate::capture::CaptureRequest {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: crate::types::WikiId::parse("alice").unwrap(),
                page: None,
                body: "Alice ha comprato del concime.".to_owned(),
                subject: "user:alice".parse::<crate::types::Principal>().unwrap(),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".to_owned()),
                topics: Vec::new(),
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            },
            None,
        )
        .await
        .unwrap();

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"style\":\"prosa\",\"description\":\"Everything about the garden\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\",\"alice/compost.md\"]}]}",
        );
        let cronista_llm = FakeLlmBackend::new(
            "cro",
            "{\"mergedBody\":\"Una nota.\",\"description\":\"Le note dell'orto.\",\"style\":\"prosa\"}",
        );
        let llms = RemLlms {
            revisor: &rev_llm,
            auto_promote: Some(&promote_llm),
            apply: None,
            comment_applier: None,
            cronista: Some(&cronista_llm),
            navigator: None,
        };
        // The whole night, not the cycle alone: the closing pass and the
        // build that follow it rebuild the plan from the store, and that is
        // where the promotion has to still be standing.
        crate::dream::run_full(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        // The source wiki is left with the page that was not the subject: one
        // of the three reappearing here is the compile having undone the
        // promotion.
        let source = tree.wikis_dir().join("alice");
        let came_back: Vec<String> = std::fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "_meta.md" && !n.starts_with("bollette"))
            .collect();
        assert!(
            came_back.is_empty(),
            "the compile put pages back where they came from: {came_back:?}",
        );

        // The registry is the half the compile actually reads, and the
        // promotion rewrote the three entries that named the source wiki. The
        // rebuild that follows may re-plan the concepts, but nothing it plans
        // may still be pointing back where the pages came from.
        let reg =
            crate::planner::load_concept_registry(&tree, &chrono::Utc::now().to_rfc3339()).unwrap();
        assert!(!reg.entries.is_empty(), "the rebuild registered the pages");
        let left_behind: Vec<&str> = reg
            .entries
            .values()
            .filter(|e| e.wiki_id == "alice" && !e.slug.starts_with("bollette"))
            .map(|e| e.slug.as_str())
            .collect();
        assert!(
            left_behind.is_empty(),
            "the registry still sends concepts to the wiki the pages left: {left_behind:?}",
        );

        let rows = fact_index::find_active_in_wiki(&pool, "giardino")
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            6,
            "every fact still lives in the new wiki after the compile — a page \
             whose facts point elsewhere is served as redacted",
        );
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
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"style\":\"prosa\",\"description\":\"Everything about the garden\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\",\"alice/compost.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_wikis_examined, 1);
        assert_eq!(report.auto_promote.grouping_groups_applied, 1);
        assert_eq!(report.auto_promote.applied.len(), 1);

        // The wiki is born holding all three pages under their own names.
        let new_dir = tree.wikis_dir().join("giardino");
        assert!(new_dir.join("_meta.md").exists(), "sub-wiki must exist");
        for page in ["orto.md", "potatura.md", "compost.md"] {
            assert!(new_dir.join(page).exists(), "{page} must have moved in");
            assert!(
                !tree.wikis_dir().join("alice").join(page).exists(),
                "{page} must be gone from the parent",
            );
        }
        // And nothing else: a topic wiki carries the pages it was founded
        // on plus its own `_meta.md`, and coins no page of its own.
        let mut born: Vec<String> = std::fs::read_dir(&new_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        born.sort();
        assert_eq!(
            born,
            vec!["_meta.md", "compost.md", "orto.md", "potatura.md"],
            "a topic wiki carries its pages and nothing else"
        );

        let rows = fact_index::find_active_in_wiki(&pool, "giardino")
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
        assert_eq!(ctx["variant"], "pages_to_new_wiki");
        // No source wiki on the context, and that is the change: a group is
        // named across the memory, so its pages carry their own wiki and the
        // birth answers to none of them.
        assert!(ctx["source_wiki_id"].is_null());
        assert_eq!(ctx["pages"][0], "alice/orto.md");
        assert_eq!(ctx["group_pages"], 3);
        assert_eq!(ctx["new_wiki_style"], "prosa");
        assert_eq!(ctx["new_wiki_description"], "Everything about the garden");

        let notices: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(notices, 0, "a wiki being born notices nobody");
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
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The model cut a two-page group; the floor is three. A wiki is
        // never born for a pair — they stay where they are.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
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

    /// A group whose subject already has a wiki goes in there, not into a
    /// second home for the same thing.
    ///
    /// The destination may be any wiki, not only one under the pages' current
    /// shelf — a shelf is structure, and the fact carries its own audience
    /// across. What this pass does NOT do is rescue a single stray page: it
    /// only ever sees groups the engine nominated, and a handle tying fewer
    /// pages than the floor nominates nothing. Refiling one misplaced fact is
    /// [`run_refile_sweep`]'s job, which nominates by cosine and has no floor.
    #[tokio::test]
    async fn page_grouping_files_a_group_into_a_wiki_that_already_exists() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_subwiki(&tree, "alice", "giardino", "Giardino");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"move\",\"target\":\"alice-giardino\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\",\"alice/compost.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
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
        assert_eq!(rows.len(), 6, "every fact followed its page");

        let proposal_id = &report.auto_promote.applied[0];
        let (context,): (String,) =
            sqlx::query_as("SELECT context FROM structure_proposals WHERE proposal_id = ?")
                .bind(proposal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "pages_into_wiki");
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

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
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

    /// A page whose NAME the engine decides is never offered to the grouping
    /// model, so it can never be carried into a sub-wiki. Each of them is
    /// found by its path — the rules page is opened at the wiki root, the
    /// signposts and the diary are read by path, the identity card is served
    /// from the wiki it belongs to — so a move one level down would leave the
    /// content intact and the delivery silently dead.
    #[tokio::test]
    async fn page_grouping_never_offers_a_page_the_engine_names() {
        const RESERVED: [&str; 4] = [
            crate::wiki::RULES_FILENAME,
            crate::wiki::PROJECTS_FILENAME,
            crate::wiki::PROJECT_DIARY_FILENAME,
            crate::wiki::PROFILE_FILENAME,
        ];
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        for reserved in RESERVED {
            plant_on_page(&tree, &pool, "alice", reserved, 2, "alice").await;
        }

        // Every reserved page carries facts, so nothing upstream of the
        // fence keeps it out of the mass map.
        let alice_dir = tree.wikis_dir().join("alice");
        for reserved in RESERVED {
            assert!(alice_dir.join(reserved).exists(), "{reserved} was planted");
        }

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The model asks for a reserved page by name. It is not in the
        // inventory it was shown, so the group names a page this wiki does
        // not offer and is refused whole — which is what the fence buys: the
        // grouping cannot move a page it was never allowed to see.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"style\":\"prosa\",\"description\":\"Everything about the garden\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\",\"alice/@rules.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();
        assert_eq!(
            report.auto_promote.grouping_groups_applied, 0,
            "the group named a page the wiki does not offer",
        );

        for reserved in RESERVED {
            assert!(
                alice_dir.join(reserved).exists(),
                "{reserved} must still be at its wiki's root",
            );
            assert!(
                !alice_dir.join("giardino").join(reserved).exists(),
                "{reserved} must not have been carried into the sub-wiki",
            );
        }
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_rejects_a_group_naming_a_page_the_wiki_lacks() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        plant_distinct(&tree, &pool, "alice", 2, "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // A page the model names but the wiki does not have invalidates the
        // whole group rather than carrying the two that do exist.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"pages\":[\"alice/orto.md\",\"alice/potatura.md\",\"alice/inesistente.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_groups_applied, 0);
        assert!(report.auto_promote.applied.is_empty());
        assert!(
            !tree.wikis_dir().join("alice").join("giardino").exists(),
            "and no sub-wiki was founded from the invalid group",
        );
        drop(dir);
    }

    /// A topic wiki is not asked to emerge again.
    ///
    /// The floor is a count of pages, and the pages a birth carried are still
    /// there the next night: the same handle ties them again and the question
    /// comes back for as long as the memory holds. Both answers left to it are
    /// wrong — a second wiki for one argument, or a move into the wiki the
    /// pages are already in, which the apply refuses. So the group never
    /// reaches the model, and the pass says it asked nothing.
    #[tokio::test]
    async fn a_group_that_is_a_whole_wiki_is_never_asked_about() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "giardino", "Giardino", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // The shape a birth leaves behind: every page of this wiki shares the
        // handle, and the wiki holds nothing else.
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "giardino", page, 2, "alice").await;
        }

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // Scripted to say yes. It is never called: proving the drop is the
        // engine's and not the model's.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino-2\",\"title\":\"Giardino\",\
             \"pages\":[\"giardino/orto.md\",\"giardino/potatura.md\",\"giardino/compost.md\"]}]}",
        );
        let llms = grouping_llms(&rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(
            report.auto_promote.grouping_wikis_examined, 0,
            "the group never reached the model",
        );
        assert_eq!(report.auto_promote.grouping_groups_applied, 0);
        assert!(
            !tree.wikis_dir().join("giardino-2").exists(),
            "and no second wiki was founded for an argument that has one",
        );
        // A page of another subject makes it a real question again: those
        // three stop being everything the wiki holds.
        plant_off_topic_page(&tree, &pool, "giardino", "alice").await;
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();
        assert_eq!(
            report.auto_promote.grouping_wikis_examined, 1,
            "nine of fifteen pages is a question; fifteen of fifteen is not",
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
        plant_off_topic_page(&tree, &pool, "alice", "alice").await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // A tidy wiki: the model finds nothing worth grouping.
        let promote_llm = FakeLlmBackend::new("rp", "{\"groups\":[]}");
        let llms = grouping_llms(&rev_llm, &promote_llm);
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

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&rev_llm),
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
        assert!(path.ends_with("alice/preferenze.md"), "got path {path:?}");
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&rev_llm),
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
        // Plant an overdue `dedup_merge` row: `timeout_at` an hour ago,
        // so the sweep picks it up but the grace window is still open.
        let proposal_id =
            plant_pending_dedup_merge(&pool, winner.as_str(), loser.as_str(), -3600).await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&rev_llm),
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
        // The sweep lands the row straight on `applied` with
        // `apply_mode='auto'` — there is no confirmation to wait for.
        let (status, apply_mode): (String, Option<String>) = sqlx::query_as(
            "SELECT status, apply_mode FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert_eq!(apply_mode.as_deref(), Some("auto"));
        let event_kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM wiki_events ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            event_kinds.is_empty(),
            "the nightly cycle tells the user nothing, got {event_kinds:?}",
        );
        drop(dir);
    }

    /// A row the auto-apply sweep cannot apply stops being retried once
    /// the grace window past `timeout_at` closes: the expire sweep that
    /// runs right after it in the same cycle flips the row to `expired`.
    #[tokio::test]
    async fn auto_apply_sweep_expires_a_row_past_grace() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Fact ids nothing planted: the handler refuses, so the row is
        // still `pending` when the expire sweep looks at it.
        let proposal_id = plant_pending_dedup_merge(
            &pool,
            "fact-01J0000000000000000000WIN",
            "fact-01J0000000000000000000LOS",
            -(48 * 3600),
        )
        .await;

        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        assert!(report.auto_apply.applied.is_empty(), "the handler refuses");
        assert_eq!(report.auto_apply.expired, 1);

        let status: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&proposal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "expired");
        drop(dir);
    }

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

    /// Materialise a smart wiki on disk + sync the registry.
    /// Mirrors `write_wiki` but stamps the `wiki-companion` type and the
    /// `smart: true` flag that goes with it.
    /// Returns the wiki id so callers can plant facts in it.
    fn write_smart_wiki(tree: &WikiTree, slug: &str, title: &str, owner: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-companion\nslug: {slug}\ntitle: {title}\nacl_default: 'user:{owner}'\nsmart: true\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# placeholder page\n").unwrap();
    }

    #[tokio::test]
    async fn briefing_dispatcher_emits_stale_draft_notify_for_smart_wiki() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_smart_wiki(&tree, "alice-lnprint", "lnprint smart wiki", "alice");
        tree = WikiTree::open(dir.path()).unwrap();

        let draft = "status: draft\ntopic: MFA recovery codes\nbody: 'TODO write up'\n";
        let handle = plant_section(&pool, "alice-lnprint", draft).await;

        // Stale-draft window of -1 ns ⇒ threshold = now + 1 ns, so the
        // just-planted fact is unambiguously "older" than the threshold.
        let policy = RemPolicy {
            briefing_stale_draft_age: chrono::Duration::nanoseconds(-1),
            ..RemPolicy::default()
        };
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        write_smart_wiki(&tree, "alice-lnprint", "lnprint smart wiki", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        let draft = "status: draft\ntopic: stale\nbody: 'TODO'\n";
        let _ = plant_section(&pool, "alice-lnprint", draft).await;
        let policy = RemPolicy {
            briefing_stale_draft_age: chrono::Duration::nanoseconds(-1),
            ..RemPolicy::default()
        };
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");

        let r1 = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
            .await
            .expect("cycle 1");
        assert_eq!(r1.briefing_dispatcher.notifications_emitted.len(), 1);
        assert_eq!(r1.briefing_dispatcher.deduplicated, 0);

        let r2 = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
            .await
            .unwrap();
        assert_eq!(report.briefing_dispatcher.wikis_examined, 0);
        assert!(report.briefing_dispatcher.notifications_emitted.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn legacy_write_jobs_skip_smart_family() {
        let (dir, mut tree, pool) = setup_workdir().await;
        // Smart wiki with children + an active fact ⇒ would normally
        // qualify for a write job; the smart-family gate must filter it out.
        let parent = "alice-lnprint";
        let child = "alice-lnprint-auth";
        let parent_dir = tree.wikis_dir().join(parent);
        std::fs::create_dir_all(&parent_dir).unwrap();
        let fm = format!(
            "---\nwiki_id: {parent}\nwiki_type: wiki-companion\nslug: {parent}\ntitle: lnprint\nacl_default: 'user:alice'\nsmart: true\nchildren:\n  - wiki_id: {child}\n    slug: auth\n    title: Auth\n    wiki_type: wiki-tech\n---\n"
        );
        std::fs::write(parent_dir.join("_meta.md"), fm).unwrap();
        std::fs::write(parent_dir.join("index.md"), "# smart-original\n").unwrap();
        write_wiki(&tree, child, "Auth", "wiki-tech");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, parent, "active fact body", "alice").await;

        let policy = RemPolicy::default();
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
            .await
            .expect("cycle");

        let index = std::fs::read_to_string(parent_dir.join("index.md")).unwrap();
        assert!(
            index.contains("smart-original"),
            "smart-wiki index.md must remain untouched, got {index:?}",
        );
        drop(dir);
    }

    // ---------- Briefing-processor non-smart ----------

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
            Some("wiki://alice/preferenze.md"),
        )
        .await;

        let policy = RemPolicy::default();
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
            "processed_at must be stamped after the briefing processor ran, got {processed:?}"
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
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/preferenze.md".to_owned(),
                region_start: Some(0),
                region_end: Some(20),
                text: "Alice was born in 1985".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                subject_id: "user:alice".parse::<crate::types::Principal>().unwrap(),
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
            Some("wiki://alice/preferenze.md#bio"),
        )
        .await;

        let policy = RemPolicy::default();
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let applier = FakeLlmBackend::new(
            "ingest",
            format!(
                "{{\"ops\":[{{\"action\":\"correct\",\"fact_id\":\"{}\",\"text\":\"Alice was born in 1986\"}}]}}",
                fid.as_str()
            ),
        );
        let llms = RemLlms {
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
        write_smart_wiki(&tree, "alice-lnprint", "lnprint smart wiki", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        let bi_id =
            insert_pending_briefing_row(&pool, "alice-lnprint", chrono::Duration::hours(48), None)
                .await;

        let policy = RemPolicy::default();
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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

    // ---------- structural review ----------

    /// What the judge is shown, and in what order.
    ///
    /// Two claims, and the second is the one that makes the cap safe. The
    /// inventory reports **who each page's facts are about** — not the page's
    /// name and not the wiki it happens to sit in, because those are exactly
    /// what may be wrong. And when the forest does not fit, what it keeps is
    /// the pages whose facts point away from their own wiki: the cheap signal
    /// for the mistake, so what falls off the end is what nothing suggests is
    /// worth looking at.
    #[test]
    fn the_forest_inventory_reports_subjects_and_puts_the_odd_pages_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        write_wiki(&tree, "franz", "Franz", "wiki-user");

        let page = |slug: &str, subjects: &[&str]| {
            let facts: Vec<crate::planner::FactForPage> = subjects
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let id = format!(
                        "0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{:02x}",
                        i + slug.len() * 16
                    );
                    let mut f = crate::planner::FactForPage {
                        topics: Vec::new(),
                        subject_external: None,
                        authored_refs: Vec::new(),
                        fact_id: FactId::parse(&id).unwrap(),
                        text: "t".to_owned(),
                        fact_type: None,
                        subject: s.parse().unwrap(),
                        allow: Vec::new(),
                        sender: None,
                        source_wiki_id: "franz".to_owned(),
                        valid_from: None,
                        valid_to: None,
                        decay_reason: None,
                        successor_fact_id: None,
                        target_page: None,
                        style: None,
                        salience: None,
                    };
                    f.text = format!("fact {i}");
                    f
                })
                .collect();
            crate::planner::PagePlan {
                title: slug.to_owned(),
                description: format!("what {slug} holds"),
                style: None,
                primary_facts: facts,
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "franz".to_owned(),
                page_path: format!("{slug}.md"),
                slug: slug.to_owned(),
            }
        };

        let mut pages = BTreeMap::new();
        // Franz's own page: its facts are about him, like its wiki.
        pages.insert("his".to_owned(), page("his", &["user:franz", "user:franz"]));
        // A page in Franz's wiki whose facts are mostly the household's.
        pages.insert(
            "theirs".to_owned(),
            page(
                "theirs",
                &["group:famiglia", "group:famiglia", "user:franz"],
            ),
        );
        // His identity card, which is his wiki's by construction.
        let mut card = page("franz", &["user:franz"]);
        card.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        pages.insert("franz".to_owned(), card);

        let plan = crate::planner::CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "2026-08-25T00:00:00Z".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        let inv = forest_inventory(&tree, &plan);
        let seen: Vec<&str> = inv.iter().map(|p| p.address.as_str()).collect();
        assert_eq!(
            seen,
            vec!["franz/theirs.md", "franz/his.md"],
            "the card is never a candidate, and the odd page sorts first: {seen:?}"
        );
        assert_eq!(
            inv[0].subjects,
            vec![
                ("group:famiglia".to_owned(), 2),
                ("user:franz".to_owned(), 1)
            ],
            "the judge weighs who the facts are ABOUT, biggest first"
        );
        assert!(inv[0].off_principal, "its facts point away from its wiki");
        assert!(!inv[1].off_principal, "his own page points at him");
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
            &BTreeSet::new(),
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
        assert_eq!(notices, 0, "the nightly cycle notices nobody");
        drop(dir);
    }

    /// Rules-page facts sit outside the completion model on both axes: a
    /// standing directive completes nothing and is never completed by
    /// neighbouring evidence (the 2026-07-05 live incident: franz's
    /// "Gandalf" naming rule read as evidence completing morgana's
    /// parallel "Ernest" naming rule — cross-user collateral).
    /// The other way an intention ends: it is called off.
    ///
    /// This is the only pass that DISCOVERS a closure — the contradiction
    /// sweep starts from one somebody already made — so when it knew only
    /// "it happened", a commitment cancelled in a turn whose recall window
    /// did not hold it stayed open for ever, and the memory kept telling the
    /// agent about an appointment nobody was going to keep.
    #[tokio::test]
    async fn completion_sweep_closes_an_abandoned_item_as_retracted() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let open_item = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il 24 giugno deve aiutare un amico a spostare una lavatrice",
            "alice",
        )
        .await;
        let evidence = plant_fact(
            &tree,
            &pool,
            "alice",
            "Ha detto all'amico che non può andare: l'impegno è annullato",
            "alice",
        )
        .await;
        assert_ne!(open_item, evidence);
        let resp = format!(
            "{{\"completions\":[{{\"target\":\"{}\",\"outcome\":\"retracted\",\"valid_to\":null}}]}}",
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
            &BTreeSet::new(),
        )
        .await
        .expect("sweep");
        assert_eq!(report.closed, vec![open_item.as_str().to_owned()]);
        let row = fact_index::find_by_id(&pool, &open_item)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_some(), "the window closes either way");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::RETRACTED),
            "an abandoned intention did not happen: closing it as completed \
             would say the opposite of what the evidence says"
        );
        drop(dir);
    }

    /// The window closes at the instant the EVIDENCE began, not at the wall
    /// clock of the cycle that read it.
    ///
    /// The evidence IS what closed the item, so the item ends where its
    /// replacement starts. On a live turn the two instants are minutes apart
    /// and the difference is invisible; on a backlog replay every fact is
    /// captured tonight and none of them happened tonight, so `created_at`
    /// dates a June errand to the August evening the engine caught up.
    #[tokio::test]
    async fn a_completion_closes_when_the_evidence_began_not_when_it_was_read() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let open_item = plant_fact(&tree, &pool, "alice", "Vuole vedere Jumanji", "alice").await;
        let began = (Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        let evidence = plant_fact_with_window(
            &tree,
            &pool,
            "alice",
            "Hanno visto Jumanji ieri sera",
            "alice",
            Some(began.clone()),
            None,
        )
        .await;
        assert_ne!(open_item, evidence);

        // `valid_to: null` — the confirmer resolved no date of its own, which
        // is the arm where the engine supplies one.
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
            &BTreeSet::new(),
        )
        .await
        .expect("sweep");

        assert_eq!(report.closed, vec![open_item.as_str().to_owned()]);
        let row = fact_index::find_by_id(&pool, &open_item)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.valid_to.as_deref(),
            Some(began.as_str()),
            "the item ends where the evidence that closed it began"
        );
        drop(dir);
    }

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
                        subject_external: None,
                        slot: None,
                        slot_value: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("bot").unwrap(),
                        page: Some(PathBuf::from("@rules.md")),
                        body,
                        subject: Principal::User("bot".to_owned()),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
    /// redundant receipt — which also keeps the single receipt's
    /// prior-state snapshot honest (`close_validity` has no re-close guard
    /// of its own).
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
            &BTreeSet::new(),
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

    /// **A fact filed away from its own subject is offered its subject's
    /// wiki, whatever the vectors say.**
    ///
    /// This is the case the sweep exists for, and a similarity ranking is the
    /// one thing that cannot find it: a fact about Bob does not have to
    /// *sound* like Bob's other facts. Here it sounds like nothing in Bob's
    /// wiki at all — the cosine margin refuses it outright — and it is offered
    /// anyway, tagged with the reason.
    #[tokio::test]
    async fn a_fact_is_offered_its_own_subjects_wiki_even_when_nothing_there_sounds_like_it() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        // Alice's wiki and the misfiled fact sit on ONE axis; Bob's wiki sits
        // on another. By cosine, the fact is exactly where it belongs.
        plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Alice loves pasta",
            "alice",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        // Two of Bob's own, so his wiki has an internal similarity of its own
        // and nothing there is nominated to leave.
        for body in ["Bob plays the trumpet", "Bob rehearses on Thursdays"] {
            plant_fact_with_embedding(&tree, &pool, "bob", body, "bob", vec![0.0, 1.0, 0.0, 0.0])
                .await;
        }
        let misfiled = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Bob is allergic to penicillin",
            "bob",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let llm = FakeLlmBackend::new("confirmer", "{\"verdict\":\"stay\",\"reason\":\"x\"}");
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-subject",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert_eq!(
            report.candidates_examined, 1,
            "the misfiled fact is nominated: {report:?}"
        );
        let shown = llm.last_prompt().expect("the confirmer was asked");
        assert!(shown.contains("penicillin"), "{shown}");
        assert!(
            shown.contains("[same-people] bob"),
            "bob's wiki is offered, and the line says why: {shown}"
        );
        let _ = misfiled;
        drop(dir);
    }

    /// Plant one fact onto a page with its KIND and the day it began spelled
    /// out. The judge reasons about what a fact IS and when it was true, so
    /// its tests have to set both — and it reads PAGES, so they all land on
    /// one.
    async fn plant_on_page_of_kind(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        body: &str,
        subject: &str,
        fact_type: &str,
        valid_from: Option<String>,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some(fact_type.to_owned()),
            topics: Vec::new(),
            dedup_threshold: Some(0.999),
            valid_from,
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

    /// Back-date a planted fact, so a test about "the newest first" says
    /// which one is newest instead of hoping two writes land a millisecond
    /// apart.
    async fn written_at(pool: &SqlitePool, fact_id: &FactId, created_at: &str) {
        sqlx::query("UPDATE fact_index SET created_at = ? WHERE fact_id = ?")
            .bind(created_at)
            .bind(fact_id.as_str())
            .execute(pool)
            .await
            .expect("back-date");
    }

    /// Run the judge over the page these facts live on.
    async fn judge_the_page(
        pool: &SqlitePool,
        tree: &WikiTree,
        llm: &dyn LlmBackend,
        fresh: &[FactId],
        depth: JudgementDepth,
        policy: &RemPolicy,
    ) -> PageJudgementReport {
        let to_judge = pages_the_round_wrote_onto(pool, fresh, depth.page_cap(policy)).await;
        run_page_judgement(
            pool,
            tree,
            fake_embedder(),
            Some(llm),
            "cycle-test",
            Utc::now(),
            policy,
            &to_judge,
            depth,
        )
        .await
        .expect("page judgement")
    }

    /// **The page arrives as prose, and every fact on it can be named.**
    ///
    /// This is the whole mechanism: the words the Cronista wrote, with each
    /// live fact wrapped in the marker the model answers by, and the facts
    /// that have not reached the prose yet listed after it so nothing on the
    /// page is unnameable. The numbering follows the READING order, because
    /// that is the order the model sees.
    #[test]
    fn a_page_is_rendered_as_prose_with_every_fact_named() {
        let older = FactId::parse("018f1234-5678-7abc-9def-000000000001").unwrap();
        let newer = FactId::parse("018f1234-5678-7abc-9def-000000000002").unwrap();
        let unwoven = FactId::parse("018f1234-5678-7abc-9def-000000000003").unwrap();
        let row = |id: &FactId, text: &str| FactIndexRow {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            fact_id: id.clone(),
            wiki_id: "bob".to_owned(),
            source_path: "wikis/bob/casa.md".to_owned(),
            region_start: None,
            region_end: None,
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            subject_id: "user:bob".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("episode".to_owned()),
            topics: Vec::new(),
            created_at: "2026-03-14T18:00:00Z".to_owned(),
            updated_at: "2026-03-14T18:00:00Z".to_owned(),
            superseded_at: None,
            superseded_by: None,
            successor_fact_id: None,
            deleted_at: None,
            deleted_reason: None,
            last_recall_at: None,
            recall_count_30d: 0,
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        };
        // The prose carries the two woven facts in the other order: the
        // markers must follow the page, not the database.
        let body = "Pepper is the household cat. {{f=018f1234-5678-7abc-9def-000000000002}}She \
                    has been fed.{{/}} {{f=018f1234-5678-7abc-9def-000000000001}}Alice asked \
                    somebody to feed the cat.{{/}}\n";
        let (rendered, ordered) = render_page_with_markers(
            body,
            vec![
                row(&older, "Alice asked somebody to feed the cat."),
                row(&newer, "She has been fed."),
                row(&unwoven, "The vet is on Tuesday."),
            ],
        );

        assert!(
            rendered.contains("Pepper is the household cat."),
            "the prose between the facts is what makes them readable: {rendered}"
        );
        assert!(
            rendered.contains("<f1>She has been fed.</f1>"),
            "first in the PAGE is f1: {rendered}"
        );
        assert!(
            rendered.contains("<f2>Alice asked somebody to feed the cat.</f2>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<f3>The vet is on Tuesday.</f3>"),
            "a fact the prose has not taken yet is listed, never hidden: {rendered}"
        );
        assert!(
            !rendered.contains("{{f="),
            "the runtime markers never reach the model: {rendered}"
        );
        assert_eq!(
            ordered
                .iter()
                .map(|f| f.fact_id.as_str())
                .collect::<Vec<_>>(),
            vec![newer.as_str(), older.as_str(), unwoven.as_str()],
            "the engine maps f1, f2, f3 back in that order"
        );
    }

    /// **An errand is true of its day and of no other.**
    ///
    /// The classifier sees one turn. «Pepper has been fed» arrives with no
    /// end, and a fact with no end is a STANDING state: the page goes on
    /// saying in September that the cat has been fed, about a meal in March,
    /// and recall keeps offering it as news. The judge reads the page and
    /// gives the fact the day it belongs to.
    ///
    /// It gives it a DAY, not a death: nothing here decayed, the fact was only
    /// ever true of one day, so `decay_reason` stays empty. That is what
    /// separates this verb from the closure in the test below, where somebody
    /// really did finish something.
    #[tokio::test]
    async fn the_judge_ends_an_errand_on_the_day_it_happened() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let errand = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Pepper has been fed",
            "bob",
            "episode",
            Some("2026-03-14T18:00:00Z".to_owned()),
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-03-14\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&errand),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(report.pages_read, 1);
        assert_eq!(report.changed, vec![format!("{} · end", errand.as_str())]);
        assert_eq!(report.receipts.len(), 1, "one receipt for the page");
        let row = fact_index::find_by_id(&pool, &errand)
            .await
            .unwrap()
            .expect("row");
        assert!(
            row.valid_to
                .as_deref()
                .is_some_and(|v| v.starts_with("2026-03-14")),
            "the day the errand happened, not the day it was read: {:?}",
            row.valid_to
        );
        assert!(
            row.decay_reason.is_none(),
            "nothing closed this fact — it was only ever true of its day"
        );
        assert!(
            row.deleted_at.is_none() && row.superseded_at.is_none(),
            "the judge ends a window, it never erases a fact"
        );
        let (status, kind): (String, String) =
            sqlx::query_as("SELECT status, kind FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt");
        assert_eq!(status, "applied", "act first, receipt after");
        assert_eq!(kind, proposals::kind::PAGE_JUDGED);
        drop(dir);
    }

    /// **One fact on a page finishes another.**
    ///
    /// Somebody asked for the cat to be fed and somebody else, an hour later,
    /// said it was done. The two turns never met: the classifier that read the
    /// answer had no idea a request existed, so the request stayed open and
    /// the household's memory went on saying the cat needed feeding. On the
    /// page they are two sentences one after the other, and that is all it
    /// takes.
    ///
    /// The answer itself stays open: it is the evidence, not the thing that
    /// ended. A pass that closed both would be closing a fact nobody said
    /// anything about.
    #[tokio::test]
    async fn one_fact_on_the_page_closes_another() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let request = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Qualcuno deve dare da mangiare a Pepper",
            "bob",
            "plan",
            None,
        )
        .await;
        let answer = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Pepper has been fed",
            "bob",
            "episode",
            None,
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"closes\",\"target\":\"f1\",\"reason\":\"completed\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&answer),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(
            report.changed,
            vec![format!("{} · closes", request.as_str())]
        );
        let closed = fact_index::find_by_id(&pool, &request)
            .await
            .unwrap()
            .expect("request row");
        assert!(closed.valid_to.is_some(), "the request is finished");
        assert_eq!(
            closed.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
        assert_eq!(
            closed.successor_fact_id.as_ref().map(FactId::as_str),
            Some(answer.as_str()),
            "the page can say what finished it"
        );
        let evidence = fact_index::find_by_id(&pool, &answer)
            .await
            .unwrap()
            .expect("answer row");
        assert!(
            evidence.valid_to.is_none(),
            "the answer closed the request, not itself"
        );
        drop(dir);
    }

    /// **A role that runs out is not who somebody is.**
    ///
    /// Filed as `bio`, a fact is always-on: it rides on the identity card, it
    /// is offered on every turn, and it never ends. A mortgage, a course, a
    /// job for the season all read like traits when the classifier meets them
    /// in one sentence, and each one of them is a thing that finishes. The
    /// judge files the fact as `state` and gives it the end it has — one
    /// verdict, two corrections, because either alone leaves the fact wrong.
    #[tokio::test]
    async fn a_role_that_runs_out_is_refiled_as_a_passing_state() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "zoe", "Zoe", "wiki-user");
        let mortgage = plant_on_page_of_kind(
            &tree,
            &pool,
            "zoe",
            "casa.md",
            "Zoe paga il mutuo fino ad aprile 2031",
            "zoe",
            "bio",
            Some("2026-04-01T00:00:00Z".to_owned()),
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"retype\",\"fact_type\":\"state\",\"valid_to\":\"2031-04-30\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&mortgage),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(
            report.changed,
            vec![format!("{} · retype", mortgage.as_str())]
        );
        let row = fact_index::find_by_id(&pool, &mortgage)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.fact_type.as_deref(),
            Some("state"),
            "what somebody is doing, not who they are"
        );
        assert!(
            !fact_index::is_an_identity_kind(row.fact_type.as_deref()),
            "it is off the identity card from now on"
        );
        assert!(
            row.valid_to
                .as_deref()
                .is_some_and(|v| v.starts_with("2031-04-30")),
            "with the end it was always going to have: {:?}",
            row.valid_to
        );
        drop(dir);
    }

    /// **A page the judge agrees with is not bought twice — and the night
    /// reads it anyway.**
    ///
    /// «Ha la fobia dei ragni» is `bio` and stays `bio`: it is exactly what an
    /// identity card is for. What costs money is asking again, and this pass
    /// runs every hour and again at night over pages nothing happened to — so
    /// a page that came back all-`keep` is remembered as it stands.
    ///
    /// Remembered per PASS, though. The night asks seven questions where the
    /// hour asks three, and a smaller «no» must not answer for a bigger
    /// question.
    #[tokio::test]
    async fn a_page_the_judge_agrees_with_is_not_bought_twice() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let trait_fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "profilo.md",
            "Ha la fobia dei ragni",
            "alice",
            "bio",
            None,
        )
        .await;

        let llm = FakeLlmBackend::new("judge", "{\"verdicts\":{}}");
        let first = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&trait_fact),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(first.pages_read, 1);
        assert!(first.changed.is_empty(), "{:?}", first.changed);
        assert!(
            first.receipts.is_empty(),
            "nothing happened, nothing to show"
        );

        let second = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&trait_fact),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            second.pages_read, 0,
            "the same question about the same page is not asked twice"
        );
        assert_eq!(llm.max_tokens_seen().len(), 1, "one call, not one per hour");

        let tonight = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&trait_fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            tonight.pages_read, 1,
            "the night asks two more questions of the same page"
        );

        let row = fact_index::find_by_id(&pool, &trait_fact)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.fact_type.as_deref(), Some("bio"));
        assert!(row.valid_to.is_none(), "a phobia does not end at bedtime");
        drop(dir);
    }

    /// **The markers on the page are the whole world for one call.**
    ///
    /// A verdict about `f9` on a page with two facts is a model reaching into
    /// the corpus from memory, and acting on it would let one invented marker
    /// close somebody's commitment. It is refused BY NAME — and the refusal
    /// rides on the same receipt as the change that did land, so a person
    /// reading the page's row sees both what was done and what was asked for
    /// and denied.
    #[tokio::test]
    async fn a_verdict_naming_a_marker_that_is_not_there_is_refused() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let watered = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "casa.md",
            "Ha annaffiato le piante",
            "alice",
            "episode",
            Some("2026-03-14T09:00:00Z".to_owned()),
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-03-14\"},\
              \"f9\":{\"verdict\":\"end\",\"valid_to\":\"2026-03-14\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&watered),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(report.changed, vec![format!("{} · end", watered.as_str())]);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert!(
            report.refused[0].contains("no such marker"),
            "{:?}",
            report.refused
        );
        let (context,): (String,) =
            sqlx::query_as("SELECT context FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt");
        assert!(
            context.contains("no such marker on this page"),
            "the receipt says what the judge asked for and did not get: {context}"
        );
        drop(dir);
    }

    /// **The hour is not asked to weigh two facts against each other.**
    ///
    /// The shorter prompt asks three questions; this is the engine saying the
    /// same thing, so a model that answers `contradicted` out of habit on the
    /// cheap reading cannot retire a fact with it. The night, which asks the
    /// question, applies it.
    #[tokio::test]
    async fn the_hourly_pass_refuses_the_two_verdicts_it_was_not_asked() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let at_the_office = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "lavoro.md",
            "Alice è in ufficio oggi",
            "alice",
            "state",
            None,
        )
        .await;
        let from_home = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "lavoro.md",
            "Alice lavora da casa tutta la settimana",
            "alice",
            "state",
            None,
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"contradicted\",\"by\":\"f2\"}}}",
        );
        let hourly = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&from_home),
            JudgementDepth::Hourly,
            &RemPolicy::default(),
        )
        .await;
        assert!(hourly.changed.is_empty(), "{:?}", hourly.changed);
        assert!(
            hourly.refused[0].contains("not asked to weigh"),
            "{:?}",
            hourly.refused
        );
        assert!(
            fact_index::find_by_id(&pool, &at_the_office)
                .await
                .unwrap()
                .expect("row")
                .valid_to
                .is_none(),
            "nothing closed on the hourly reading"
        );

        let nightly = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&from_home),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            nightly.changed,
            vec![format!("{} · contradicted", at_the_office.as_str())],
            "the pass that asks the question is the pass that may act on it"
        );
        let closed = fact_index::find_by_id(&pool, &at_the_office)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            closed.decay_reason.as_deref(),
            Some(fact_index::decay::CONTRADICTED)
        );
        drop(dir);
    }

    /// **The newer copy wins because it carries what the later turn added —
    /// and when it does not, it wins nothing.**
    ///
    /// The rule that picks the survivor rests on a sentence that has to be
    /// true: a claim said again in fewer words is a POORER copy of itself, and
    /// merging into it takes a figure out of the memory nobody asked to lose.
    /// The turn's own reconciler refuses the same shape for the same reason
    /// (`ingest::vet_supersede`), and the two share the net that says what a
    /// value is, and what it is a value OF (`compiler::orphaned_values`).
    #[tokio::test]
    async fn the_newer_copy_wins_nothing_when_it_says_less() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let richer = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "casa.md",
            "House insurance renews on 3 August. The excess has increased to £350.",
            "alice",
            "state",
            None,
        )
        .await;
        let poorer = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "casa.md",
            "House insurance renews on 3 August.",
            "alice",
            "state",
            None,
        )
        .await;
        written_at(&pool, &richer, "2026-09-11T08:00:00Z").await;
        written_at(&pool, &poorer, "2026-09-12T08:00:00Z").await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"duplicate_of\",\"target\":\"f1\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&poorer),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert!(
            report.changed.is_empty(),
            "nothing is merged: the newer copy has lost the £350 — {report:?}"
        );
        let still_there = fact_index::find_by_id(&pool, &richer)
            .await
            .unwrap()
            .expect("row");
        assert!(
            still_there.superseded_at.is_none() && still_there.deleted_at.is_none(),
            "the richer claim stands"
        );
        drop(dir);
    }

    /// **The same claim written twice becomes one.**
    ///
    /// Two turns said the same thing and the page carries it twice. The judge
    /// sees both in one reading — which is the whole difference from the dedup
    /// revisor, that fishes for pairs by vector distance — and the newer copy
    /// survives, whichever way round the model names them.
    #[tokio::test]
    async fn two_copies_of_one_claim_become_one() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let older = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "salute.md",
            "Alice è allergica alle arachidi",
            "alice",
            "state",
            None,
        )
        .await;
        let newer = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "salute.md",
            "Alice è allergica alle arachidi e porta sempre l'autoiniettore",
            "alice",
            "state",
            None,
        )
        .await;
        written_at(&pool, &older, "2026-09-11T08:00:00Z").await;
        written_at(&pool, &newer, "2026-09-12T08:00:00Z").await;

        // Named the other way round on purpose: which copy survives is the
        // engine's rule, not the model's.
        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"duplicate_of\",\"target\":\"f1\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&newer),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            report.changed,
            vec![format!("{} · duplicate_of", older.as_str())],
            "the older copy is the one retired: {report:?}"
        );
        let retired = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .expect("row");
        assert!(retired.superseded_at.is_some(), "folded into the newer one");
        assert!(
            fact_index::find_by_id(&pool, &newer)
                .await
                .unwrap()
                .expect("row")
                .superseded_at
                .is_none(),
            "the survivor stays"
        );

        drop(dir);
    }

    /// **…but never on somebody's identity card.**
    ///
    /// The other half of the test above, and the same rule the dedup revisor
    /// holds to: nothing on a person's identity core is retired while
    /// everybody is asleep. A relationship, an allergy, a name changes on an
    /// explicit correction or not at all.
    ///
    /// Here the card's copy is the one that would be RETIRED while the fact
    /// being judged is ordinary material — the case the whole-fact guard above
    /// does not see and this one does.
    #[tokio::test]
    async fn a_duplicate_on_the_identity_core_is_refused() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let core_old = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "profilo.md",
            "Carol è la sorella di Alice",
            "alice",
            "bio",
            None,
        )
        .await;
        // The newer copy is ordinary material — so it is the one that would
        // WIN the merge, and the loser is the card's. That is the case this
        // guard is for: the fact being judged is touchable, the one it would
        // retire is not.
        let core_new = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "profilo.md",
            "Carol è la sorella di Alice e vive a Bologna",
            "alice",
            "state",
            None,
        )
        .await;
        sqlx::query("UPDATE fact_index SET salience = 'high' WHERE fact_id = ?")
            .bind(core_old.as_str())
            .execute(&pool)
            .await
            .expect("mark identity core");
        written_at(&pool, &core_old, "2026-09-11T08:00:00Z").await;
        written_at(&pool, &core_new, "2026-09-12T08:00:00Z").await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"duplicate_of\",\"target\":\"f1\"}}}",
        );
        let refused = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&core_new),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert!(refused.changed.is_empty(), "{:?}", refused.changed);
        assert!(
            refused.refused[0].contains("identity core"),
            "{:?}",
            refused.refused
        );
        assert!(
            fact_index::find_by_id(&pool, &core_old)
                .await
                .unwrap()
                .expect("row")
                .superseded_at
                .is_none(),
            "a fact on the card is never retired while everybody is asleep"
        );
        drop(dir);
    }

    /// **The identity card is only ever RETYPED.**
    ///
    /// A birth date is `bio` and reserved for the card, and a night that read
    /// a page and thought it saw a contradiction could close it — the page
    /// selection lets `@profile.md` through (it must: the one correction the
    /// card needs is the mortgage filed as who somebody is), and four of the
    /// five verbs would have gone straight to `close_validity`.
    ///
    /// So the rule is on the FACT, not the page: all four closing verbs are
    /// refused by name on anything the identity core carries, wherever it
    /// lives, and `retype` — the verb that takes a fact OFF the card — is the
    /// one exception.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the five verbs against one card fact are the test: splitting them hides that this is the whole list"
    )]
    async fn a_fact_on_the_identity_card_is_only_ever_retyped() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let card_fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            crate::wiki::PROFILE_FILENAME,
            "Alice è nata il 23 maggio 1984",
            "alice",
            "bio",
            None,
        )
        .await;
        let beside_it = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            crate::wiki::PROFILE_FILENAME,
            "Alice paga il mutuo fino ad aprile 2031",
            "alice",
            "bio",
            None,
        )
        .await;
        sqlx::query("UPDATE fact_index SET salience = 'high' WHERE fact_id = ?")
            .bind(card_fact.as_str())
            .execute(&pool)
            .await
            .expect("the birth date is always-on card material");

        // Every verb that would retire it, one pass each.
        for (answer, what) in [
            (
                "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-09-13\"}}}"
                    .to_owned(),
                "end",
            ),
            (
                "{\"verdicts\":{\"f2\":{\"verdict\":\"closes\",\"target\":\"f1\",\"reason\":\"completed\"}}}"
                    .to_owned(),
                "closes",
            ),
            (
                "{\"verdicts\":{\"f1\":{\"verdict\":\"contradicted\",\"by\":\"f2\"}}}".to_owned(),
                "contradicted",
            ),
            (
                "{\"verdicts\":{\"f1\":{\"verdict\":\"duplicate_of\",\"target\":\"f2\"}}}".to_owned(),
                "duplicate_of",
            ),
        ] {
            // A model id of its own per round: the memo is keyed on it, and
            // this page comes back unchanged every time, so one name would
            // mean only the first verb was ever asked.
            let llm = FakeLlmBackend::new(format!("judge-{what}"), answer);
            let report = judge_the_page(
                &pool,
                &tree,
                &llm,
                std::slice::from_ref(&card_fact),
                JudgementDepth::Nightly,
                &RemPolicy::default(),
            )
            .await;
            assert!(
                report.changed.is_empty(),
                "`{what}` must not touch the card: {report:?}"
            );
            assert!(
                report.refused.iter().any(|r| r.contains("identity card")
                    || r.contains("identity core")),
                "`{what}` must be refused BY NAME: {report:?}"
            );
            let row = fact_index::find_by_id(&pool, &card_fact)
                .await
                .unwrap()
                .expect("row");
            assert!(
                row.valid_to.is_none()
                    && row.decay_reason.is_none()
                    && row.superseded_at.is_none(),
                "`{what}` left a mark on the card: {row:?}"
            );
        }

        // And the one verb the card IS offered to the judge for — here on a
        // `bio` fact that is NOT always-on card material, which is where the
        // correction is ordinary and lands act-first.
        let llm = FakeLlmBackend::new(
            "judge-retype",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"retype\",\"fact_type\":\"state\",\"valid_to\":\"2031-04-30\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&beside_it),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            report.changed,
            vec![format!("{} · retype", beside_it.as_str())],
            "the mortgage comes off the card: {report:?}"
        );
        assert_eq!(
            fact_index::find_by_id(&pool, &beside_it)
                .await
                .unwrap()
                .expect("row")
                .fact_type
                .as_deref(),
            Some("state")
        );
        drop(dir);
    }

    /// **On the identity core, the judge asks instead of writing.**
    ///
    /// A birth date and a secondment are both `bio` and both always-on, and
    /// from the page «this really was a passage» and «the model misread a
    /// trait» look exactly alike. One of those, applied in the night, is a
    /// person's own record rewritten while they slept — so the card's owner is
    /// asked, the memory is left as it stands, and silence keeps the card.
    ///
    /// The question goes to whoever the card belongs to, not to whoever
    /// happened to say the line — and once they have answered it, that line is
    /// never put to them again.
    #[tokio::test]
    async fn a_retype_on_the_identity_core_is_put_to_its_owner() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "zoe", "Zoe", "wiki-user");
        let core = plant_on_page_of_kind(
            &tree,
            &pool,
            "zoe",
            crate::wiki::PROFILE_FILENAME,
            "Zoe paga il mutuo fino ad aprile 2031",
            "zoe",
            "bio",
            None,
        )
        .await;
        sqlx::query("UPDATE fact_index SET salience = 'high' WHERE fact_id = ?")
            .bind(core.as_str())
            .execute(&pool)
            .await
            .expect("always-on card material");

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"retype\",\"fact_type\":\"state\",\"valid_to\":\"2031-04-30\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&core),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        assert!(
            report.changed.is_empty(),
            "nothing is written on the card: {report:?}"
        );
        assert_eq!(report.asked.len(), 1, "{report:?}");
        let row = fact_index::find_by_id(&pool, &core)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.fact_type.as_deref(), Some("bio"), "still who she is");
        assert!(row.valid_to.is_none(), "and with no end");

        // The question is pending, addressed to Zoe, and answering «keep» —
        // which is what silence applies — leaves the card alone.
        let (kind_of, status, recipient): (String, String, Option<String>) = sqlx::query_as(
            "SELECT kind, status, recipient_id FROM structure_proposals \
             WHERE json_extract(context, '$.fact_id') = ?",
        )
        .bind(core.as_str())
        .fetch_one(&pool)
        .await
        .expect("the question exists");
        assert_eq!(kind_of, proposals::kind::CARD_RETYPE);
        assert_eq!(status, "pending");
        assert_eq!(recipient.as_deref(), Some("user:zoe"));

        drop(dir);
    }

    /// **An answer is an answer for good.**
    ///
    /// The judge's own memo goes with the PAGE, and a card page changes
    /// whenever anything is written about its owner — so without a second
    /// memory, somebody who answered «keep it» would be asked again the next
    /// time an unrelated fact landed on their card. What closes the question
    /// for good is the answer, remembered against the line itself.
    #[tokio::test]
    async fn a_card_line_kept_is_never_put_to_its_owner_again() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "zoe", "Zoe", "wiki-user");
        let core = plant_on_page_of_kind(
            &tree,
            &pool,
            "zoe",
            crate::wiki::PROFILE_FILENAME,
            "Zoe paga il mutuo fino ad aprile 2031",
            "zoe",
            "bio",
            None,
        )
        .await;
        sqlx::query("UPDATE fact_index SET salience = 'high' WHERE fact_id = ?")
            .bind(core.as_str())
            .execute(&pool)
            .await
            .expect("always-on card material");
        let verdict = "{\"verdicts\":{\"f1\":{\"verdict\":\"retype\",\"fact_type\":\"state\",\"valid_to\":\"2031-04-30\"}}}";
        let llm = FakeLlmBackend::new("judge", verdict);
        let asked = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&core),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(asked.asked.len(), 1, "{asked:?}");
        let proposal_id: String = sqlx::query_scalar(
            "SELECT proposal_id FROM structure_proposals \
             WHERE json_extract(context, '$.fact_id') = ?",
        )
        .bind(core.as_str())
        .fetch_one(&pool)
        .await
        .expect("the question exists");

        // Zoe answers that it stays on her card.
        proposals::apply_proposal(
            &pool,
            &tree,
            &proposal_id,
            &serde_json::json!({"verdict": "keep"}),
            // The caller is the bare sender id, as the chat passes it.
            Some("zoe"),
            false,
            false,
        )
        .await
        .expect("answered");

        // And read again — the page's own memo is no help here, because a page
        // changes under it all the time — the judge does not put the same line
        // to her a second time.
        let llm2 = FakeLlmBackend::new(
            "judge-again",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"retype\",\"fact_type\":\"state\",\"valid_to\":\"2031-04-30\"}}}",
        );
        let again = judge_the_page(
            &pool,
            &tree,
            &llm2,
            std::slice::from_ref(&core),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert!(again.asked.is_empty(), "{again:?}");
        assert!(
            again
                .refused
                .iter()
                .any(|r| r.contains("has already been asked")),
            "{again:?}"
        );
        let questions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM structure_proposals WHERE kind = ?")
                .bind(proposals::kind::CARD_RETYPE)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(questions, 1, "one line, one question — asked once, ever");
        let row = fact_index::find_by_id(&pool, &core)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.fact_type.as_deref(),
            Some("bio"),
            "and «keep» left the card exactly as it was"
        );
        drop(dir);
    }

    /// **Same words, two audiences, two facts.**
    ///
    /// The dedup revisor refuses such a pair before it ever reaches a model,
    /// because merging them retires one person's memory and leaves the
    /// survivor addressing the other's readers. The judge does not nominate
    /// pairs — it meets them already written on a page — so the same gate has
    /// to stand on this road too, or the rule would hold only where the old
    /// pass happened to look.
    #[tokio::test]
    async fn two_copies_told_to_different_people_are_never_merged() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let shared = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "Bob prende il tè con il latte ogni mattina",
            "bob",
            vec![Principal::User("carol".to_owned())],
            None,
        )
        .await;
        let private = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "Bob prende il tè con un goccio di latte la mattina",
            "bob",
            Vec::new(),
            None,
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f2\":{\"verdict\":\"duplicate_of\",\"target\":\"f1\"}}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&private),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert!(
            report
                .refused
                .iter()
                .any(|r| r.contains("different people")),
            "refused by name: {:?}",
            report.refused
        );
        for fid in [&shared, &private] {
            assert!(
                fact_index::find_by_id(&pool, fid)
                    .await
                    .unwrap()
                    .expect("row")
                    .superseded_at
                    .is_none(),
                "neither side may be retired: {fid}"
            );
        }
        drop(dir);
    }

    /// **The cap counts pages, and it takes the newest.**
    ///
    /// One page is one model call, so the cap IS the budget. A day that wrote
    /// onto more pages than the pass may read has to choose, and the page
    /// somebody was writing on an hour ago is the one still being talked
    /// about. What it leaves is not lost and not judged: the page stays
    /// unjudged, the count says how many, and the next pass reads it.
    #[tokio::test]
    async fn the_cap_counts_pages_and_takes_the_newest() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let oldest = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "casa.md",
            "Ha chiuso a chiave",
            "alice",
            "episode",
            None,
        )
        .await;
        let middle = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "bucato.md",
            "Ha steso il bucato",
            "alice",
            "episode",
            None,
        )
        .await;
        let newest = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "rifiuti.md",
            "Ha portato fuori i bidoni",
            "alice",
            "episode",
            None,
        )
        .await;
        written_at(&pool, &oldest, "2026-09-11T08:00:00Z").await;
        written_at(&pool, &middle, "2026-09-12T08:00:00Z").await;
        written_at(&pool, &newest, "2026-09-13T08:00:00Z").await;

        let llm = FakeLlmBackend::new("judge", "{\"verdicts\":{}}");
        // The hour has its own cap, far below the night's: a pass that read
        // the wrong one would judge all three pages here.
        let policy = RemPolicy {
            judgement_max_pages_hour: 1,
            judgement_max_pages_night: 30,
            ..RemPolicy::default()
        };
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            &[oldest.clone(), middle.clone(), newest.clone()],
            JudgementDepth::Hourly,
            &policy,
        )
        .await;

        assert_eq!(report.pages_read, 1);
        assert_eq!(report.pages_left, 2, "and they are still unjudged");
        let asked = llm.last_prompt().expect("the judge was asked");
        assert!(
            asked.contains("Ha portato fuori i bidoni"),
            "the newest page got the seat: {asked}"
        );
        assert!(
            !asked.contains("Ha chiuso a chiave"),
            "the two older pages wait for the next pass: {asked}"
        );
        drop(dir);
    }

    /// A judge that takes its time — longer on the page named first — and
    /// remembers how many questions were in flight at once.
    struct SlowJudge {
        /// `(needle, answer, delay)`: the first needle the prompt contains
        /// decides both the answer and how long it takes to arrive.
        script: Vec<(&'static str, String, std::time::Duration)>,
        inside: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        answered: parking_lot::Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl LlmBackend for SlowJudge {
        fn model_id(&self) -> &'static str {
            "slow-judge"
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> std::result::Result<crate::llm::CompletionResponse, crate::llm::LlmError> {
            use std::sync::atomic::Ordering;
            let now_inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now_inside, Ordering::SeqCst);
            let (needle, answer, delay) = self
                .script
                .iter()
                .find(|(needle, _, _)| request.prompt.contains(needle))
                .cloned()
                .unwrap_or_else(|| {
                    (
                        "(none)",
                        "{\"verdicts\":{}}".to_owned(),
                        std::time::Duration::ZERO,
                    )
                });
            tokio::time::sleep(delay).await;
            self.inside.fetch_sub(1, Ordering::SeqCst);
            self.answered.lock().push(needle);
            eprintln!(
                "NEEDLE >>> {needle} for prompt containing bidoni={} bucato={}",
                request.prompt.contains("bidoni"),
                request.prompt.contains("bucato")
            );
            Ok(crate::llm::CompletionResponse {
                text: answer,
                finish_reason: crate::llm::FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage::default(),
            })
        }

        async fn chat(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> std::result::Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Invalid("not used".into()))
        }
    }

    /// **The pages travel together; their verdicts land in order.**
    ///
    /// The asking is what a night waits for, so the pages go to the model at
    /// once. The applying cannot follow suit: the verdicts of one page are
    /// applied against the page as it was READ
    /// ([`touches_a_decided_fact`]), and two pages written at once would be
    /// two writers on one `fact_index`.
    ///
    /// So this drives the two halves apart on purpose. The page the pass
    /// chooses FIRST is given the SLOWEST answer, so the model replies in the
    /// opposite order — and the changes still come out in page order.
    #[tokio::test]
    async fn the_pages_are_asked_together_and_applied_in_order() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let newest = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "bici.md",
            "Ha portato la bici dal meccanico",
            "alice",
            "episode",
            Some("2026-09-13T20:00:00Z".to_owned()),
        )
        .await;
        let oldest = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "balcone.md",
            "Ha innaffiato il basilico sul balcone",
            "alice",
            "episode",
            Some("2026-09-11T09:00:00Z".to_owned()),
        )
        .await;
        written_at(&pool, &newest, "2026-09-13T20:00:00Z").await;
        written_at(&pool, &oldest, "2026-09-11T09:00:00Z").await;

        // The needles have to be words the PROMPT does not say itself: its own
        // examples are full of errands — bins and washing included.
        let llm = SlowJudge {
            script: vec![
                (
                    "bici",
                    "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-09-13\"}}}"
                        .to_owned(),
                    std::time::Duration::from_millis(120),
                ),
                (
                    "basilico",
                    "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-09-11\"}}}"
                        .to_owned(),
                    std::time::Duration::from_millis(10),
                ),
            ],
            inside: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
            answered: parking_lot::Mutex::new(Vec::new()),
        };
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            &[newest.clone(), oldest.clone()],
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(
            llm.peak.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both pages were in flight at once"
        );
        assert_eq!(
            *llm.answered.lock(),
            vec!["basilico", "bici"],
            "and the model answered the second page first"
        );
        assert_eq!(
            report.changed,
            vec![
                format!("{} · end", newest.as_str()),
                format!("{} · end", oldest.as_str()),
            ],
            "but the verdicts were applied in page order, newest page first"
        );
        drop(dir);
    }

    /// **What the parallel asking buys, measured.**
    ///
    /// Eight pages, a fifth of a second of model latency each: asked one after
    /// another that is 1.6 s of waiting, and the whole point of the change is
    /// that a night does not spend it. The bound is deliberately loose — this
    /// runs on whatever machine CI gives it — but serial cannot pass it.
    #[tokio::test]
    async fn asking_the_pages_together_is_worth_the_wait_it_saves() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let mut fresh = Vec::new();
        for n in 0..8 {
            let id = plant_on_page_of_kind(
                &tree,
                &pool,
                "alice",
                &format!("pagina{n}.md"),
                &format!("Ha fatto la cosa numero {n}"),
                "alice",
                "episode",
                None,
            )
            .await;
            fresh.push(id);
        }
        let llm = SlowJudge {
            script: vec![(
                "Ha fatto la cosa",
                "{\"verdicts\":{}}".to_owned(),
                std::time::Duration::from_millis(200),
            )],
            inside: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
            answered: parking_lot::Mutex::new(Vec::new()),
        };
        let policy = RemPolicy {
            judgement_max_pages_night: 8,
            ..RemPolicy::default()
        };

        let started = std::time::Instant::now();
        let report =
            judge_the_page(&pool, &tree, &llm, &fresh, JudgementDepth::Nightly, &policy).await;
        let elapsed = started.elapsed();

        assert_eq!(report.pages_read, 8);
        // The bound is three quarters of serial rather than a half, because how
        // many really travel at once is the DEPLOYMENT's number and a test
        // binary shares one: at a width of two, eight pages are four waves and
        // half of serial is the floor, not a bound. Serial cannot pass this.
        let serial = std::time::Duration::from_millis(200 * 8);
        assert!(
            elapsed < serial * 3 / 4,
            "eight pages at 200ms each took {elapsed:?}; one after another is {serial:?}, \
             and this deployment asks {} at a time",
            crate::llm::in_flight_width(),
        );
        assert!(
            llm.peak.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "and they really did travel together"
        );
        drop(dir);
    }

    /// **Two depths, one pass.** The hour and the night differ in which prompt
    /// asks the questions, how many pages each may take, and whether the two
    /// verdicts that weigh one fact against another may be acted on — and in
    /// nothing else. This is that table, pinned, because every one of the four
    /// is read far from where it is set.
    #[test]
    fn the_two_depths_read_what_they_say_they_read() {
        let policy = RemPolicy {
            judgement_max_pages_night: 30,
            judgement_max_pages_hour: 5,
            ..RemPolicy::default()
        };
        assert_eq!(JudgementDepth::Hourly.page_cap(&policy), 5);
        assert_eq!(JudgementDepth::Nightly.page_cap(&policy), 30);
        assert!(
            !JudgementDepth::Hourly.weighs_facts_against_each_other(),
            "the hour asks what a page says about a fact of its own"
        );
        assert!(JudgementDepth::Nightly.weighs_facts_against_each_other());
        assert_eq!(JudgementDepth::Hourly.prompt_name(), "rem-judgement-light");
        assert_eq!(JudgementDepth::Nightly.prompt_name(), "rem-judgement");
        assert_ne!(
            JudgementDepth::Hourly.label(),
            JudgementDepth::Nightly.label(),
            "the memo key tells the two readings apart"
        );
        // The shorter prompt asks three questions and never the other two: a
        // verdict the model is not shown is one it does not return.
        assert!(!BUNDLED_REM_JUDGEMENT_LIGHT_MD.contains("duplicate_of\": {"));
        assert!(BUNDLED_REM_JUDGEMENT_MD.contains("IS THE SAME CLAIM WRITTEN TWICE"));
        assert!(!BUNDLED_REM_JUDGEMENT_LIGHT_MD.contains("IS THE SAME CLAIM WRITTEN TWICE"));
        // And the two questions about the page itself are the night's alone:
        // the hour corrects a row inside the hour, it does not reshape a page.
        assert!(!BUNDLED_REM_JUDGEMENT_LIGHT_MD.contains("{splitting}"));
        assert!(!BUNDLED_REM_JUDGEMENT_LIGHT_MD.contains("{rails}"));
        assert!(BUNDLED_REM_JUDGEMENT_MD.contains("{splitting}"));
        assert!(BUNDLED_REM_JUDGEMENT_MD.contains("{rails}"));
    }

    // ---------- page judge: what the page itself decides ----------

    /// Three facts on one page of Alice's wiki, over the mass floor
    /// [`mass_policy`] sets, so the page qualifies for the question about
    /// splitting. Returns them in the order they were written.
    async fn a_page_heavy_enough_to_split(tree: &WikiTree, pool: &SqlitePool) -> Vec<FactId> {
        let mut out = Vec::new();
        for body in [
            "Alice keeps bees on the balcony",
            "The hive is inspected on Sunday mornings",
            "The frames come from a beekeeper in Cuneo",
        ] {
            out.push(
                plant_on_page_of_kind(tree, pool, "alice", "casa.md", body, "alice", "state", None)
                    .await,
            );
        }
        out
    }

    /// **One reading answers what the page is, not only what its facts say.**
    ///
    /// Whether a page has grown a second subject is a question about the page,
    /// and the reader holding it open — the one that has just decided what each
    /// fact on it means — is the one that can answer it. It answers in the same
    /// breath as the verdicts, out of one call, and the facts it names really do
    /// move.
    #[tokio::test]
    async fn the_judge_splits_the_page_it_is_reading() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let fresh = a_page_heavy_enough_to_split(&tree, &pool).await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"split\":{\"title\":\"bees\",\"markers\":[\"f2\",\"f3\"]}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            &fresh,
            JudgementDepth::Nightly,
            &mass_policy(),
        )
        .await;

        assert_eq!(
            llm.max_tokens_seen().len(),
            1,
            "the split rode with the verdicts: ONE call for the page"
        );
        assert_eq!(
            report.changed.len(),
            1,
            "and it is the page that changed: {:?}",
            report.changed
        );
        assert!(
            report.changed[0].ends_with("· split"),
            "{:?}",
            report.changed
        );
        let where_is = async |id: &FactId| {
            fact_index::find_by_id(&pool, id)
                .await
                .unwrap()
                .expect("row")
                .source_path
        };
        assert!(
            where_is(&fresh[1]).await.ends_with("bees.md"),
            "the facts the judge named moved"
        );
        assert!(where_is(&fresh[2]).await.ends_with("bees.md"));
        assert!(
            where_is(&fresh[0]).await.ends_with("casa.md"),
            "and the one it did not name stayed"
        );
        // The same receipt the promotion pass leaves, so a reader of the
        // Proposals page sees one kind of event whichever reading decided it.
        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM structure_proposals WHERE status = 'applied'")
                .fetch_all(&pool)
                .await
                .expect("receipts");
        assert!(
            kinds.iter().any(|k| k == proposals::kind::WIKI_PROMOTE),
            "{kinds:?}"
        );
        drop(dir);
    }

    /// **Naming every fact is a rename, and a rename is not this pass's to
    /// make.**
    ///
    /// Moving the whole page leaves an empty page behind and a new one under a
    /// name nobody asked for. The engine refuses it by name, so the refusal is
    /// on the page's receipt instead of being a silent no-op.
    #[tokio::test]
    async fn a_split_that_moves_every_fact_is_refused() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let fresh = a_page_heavy_enough_to_split(&tree, &pool).await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"split\":{\"title\":\"bees\",\"markers\":[\"f1\",\"f2\",\"f3\"]}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            &fresh,
            JudgementDepth::Nightly,
            &mass_policy(),
        )
        .await;

        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert!(
            report.refused[0].contains("leaves something behind"),
            "{:?}",
            report.refused
        );
        for id in &fresh {
            let row = fact_index::find_by_id(&pool, id)
                .await
                .unwrap()
                .expect("row");
            assert!(row.source_path.ends_with("casa.md"), "{row:?}");
        }
        drop(dir);
    }

    /// **A page too light to hold two subjects is never asked, and cannot
    /// answer anyway.**
    ///
    /// The floor is a resource pre-filter: an ordinary page's prompt carries no
    /// question about splitting at all, so the model is not offered a decision
    /// nobody wants from it. And a model that answers one regardless — a prompt
    /// override in the workdir, a model repeating an example — is refused,
    /// because the fence is in the engine and not only in the wording.
    #[tokio::test]
    async fn a_page_under_the_floor_is_not_asked_to_split_and_may_not_answer() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let light = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "casa.md",
            "Alice keeps bees on the balcony",
            "alice",
            "state",
            None,
        )
        .await;

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"split\":{\"title\":\"bees\",\"markers\":[\"f1\"]}}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&light),
            JudgementDepth::Nightly,
            &mass_policy(),
        )
        .await;

        let prompt = llm.last_prompt().expect("the page was read");
        assert!(
            !prompt.contains("HAS IT GROWN A SECOND SUBJECT"),
            "a light page is not asked: {prompt}"
        );
        assert!(prompt.contains("(not asked of this page)"), "{prompt}");
        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert!(
            report.refused[0].contains("was not asked"),
            "{:?}",
            report.refused
        );
        drop(dir);
    }

    /// **The hour corrects a row; it does not reshape a page.**
    ///
    /// The short reading exists so that the turn happening now is answered from
    /// a memory that is right, and it is bought every hour. Dividing a page in
    /// two and deciding where it leads are nobody's hurry, cost a read of the
    /// whole plan, and are the night's. A model that answers them from habit
    /// finds nothing assembled to apply.
    #[tokio::test]
    async fn the_hour_reads_a_page_but_never_reshapes_it() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let fresh = a_page_heavy_enough_to_split(&tree, &pool).await;
        crate::planner::save_plan(&tree, &a_plan_of(&["casa", "orto"], "alice"))
            .expect("save plan");

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"split\":{\"title\":\"bees\",\"markers\":[\"f2\"]},\
             \"links\":[{\"for\":\"f1\",\"to\":\"orto\",\"why\":\"habit\"}]}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            &fresh,
            JudgementDepth::Hourly,
            &mass_policy(),
        )
        .await;

        assert_eq!(report.pages_read, 1, "the hour did read the page");
        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert_eq!(report.refused.len(), 2, "{:?}", report.refused);
        for id in &fresh {
            let row = fact_index::find_by_id(&pool, id)
                .await
                .unwrap()
                .expect("row");
            assert!(row.source_path.ends_with("casa.md"), "{row:?}");
        }
        let saved = crate::planner::load_previous_plan(&tree)
            .expect("load")
            .expect("plan");
        assert!(
            saved.authored_rails.is_empty(),
            "{:?}",
            saved.authored_rails
        );
        drop(dir);
    }

    /// A plan holding exactly these pages of one wiki, each with two facts and
    /// no links, so a page being judged has both a slug of its own and
    /// somewhere it could be pointed at.
    fn a_plan_of(pages: &[&str], wiki: &str) -> CompilationPlan {
        CompilationPlan {
            pages: pages
                .iter()
                .map(|slug| ((*slug).to_owned(), kin_leaf(slug, wiki, 2)))
                .collect(),
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: pages.len() * 2,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        }
    }

    /// **Where a page should lead is decided by whoever has just read it.**
    ///
    /// *«i link che il navigatore segue alla fine li ha decisi il REM»* — and
    /// the reader who has just worked out what every fact on this page means is
    /// the one who can say what somebody meeting it would need next. The link
    /// lands on the plan and leaves the same `rail_add` receipt the rail writer
    /// leaves, out of the call the page was already paying for.
    #[tokio::test]
    async fn the_judge_parks_the_rail_it_chose_in_the_same_reading() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Bob cooks without peanuts when Carol eats here",
            "bob",
            "state",
            None,
        )
        .await;
        crate::planner::save_plan(&tree, &a_plan_of(&["casa", "intolleranze"], "bob"))
            .expect("save plan");

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"links\":[{\"for\":\"f1\",\"to\":\"intolleranze\",\
             \"why\":\"a reader of this meets the allergy and cannot get to it\"}]}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        let prompt = llm.last_prompt().expect("the page was read");
        assert!(
            prompt.contains("WHICH OF ITS FACTS LEAVES A READER STUCK"),
            "{prompt}"
        );
        assert!(
            prompt.contains("- intolleranze"),
            "and it was shown where it may lead: {prompt}"
        );
        assert_eq!(
            llm.max_tokens_seen().len(),
            1,
            "the link rode with the verdicts: ONE call for the page"
        );
        assert_eq!(report.changed, vec!["casa → intolleranze · rail"]);

        let saved = crate::planner::load_previous_plan(&tree)
            .expect("load")
            .expect("plan");
        assert!(
            saved
                .authored_rails
                .contains(&("casa".to_owned(), "intolleranze".to_owned())),
            "the decision waits on the plan for the next rewrite: {:?}",
            saved.authored_rails
        );
        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM structure_proposals WHERE status = 'applied'")
                .fetch_all(&pool)
                .await
                .expect("receipts");
        assert!(kinds.iter().any(|k| k == "rail_add"), "{kinds:?}");
        drop(dir);
    }

    /// **A page nobody managed to read is nobody's answer.**
    ///
    /// The passes that stand aside for a reading stand aside for an ANSWER.
    /// A page whose call failed, or whose reply could not be read, decided
    /// nothing — so it is not on the list those passes key on, and the split
    /// and the rail writer see it that same night. Keying on the selection
    /// instead leaves it covered by nobody until tomorrow.
    #[tokio::test]
    async fn a_page_whose_reading_failed_is_not_counted_as_read() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "alice",
            "orto.md",
            "Alice keeps bees on the balcony",
            "alice",
            "state",
            None,
        )
        .await;

        // The model answers, and what comes back is not an answer.
        let llm = FakeLlmBackend::new("judge", "I could not do that");
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        assert_eq!(report.pages_read, 1, "the page was selected and opened");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.pages_answered.is_empty(),
            "but nothing was decided about it, so nobody may skip it: {:?}",
            report.pages_answered
        );

        // And the same page, read properly, IS on the list — even when every
        // answer in it was `keep`.
        let quiet = FakeLlmBackend::new("judge", "{\"verdicts\":{}}");
        let read = judge_the_page(
            &pool,
            &tree,
            &quiet,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert_eq!(
            read.pages_answered.len(),
            1,
            "a reading that changed nothing is still a reading: {read:?}"
        );
        drop(dir);
    }

    /// **«Nowhere» is an answer, and it is remembered.**
    ///
    /// Most pages need no new link, and that is the answer the reading gets.
    /// Keyed on the page's own facts, it settles the question for the rail
    /// writer too: without it that pass carries the same page to the same slot
    /// the next night, on a page nothing has touched, to be told the same
    /// thing.
    #[tokio::test]
    async fn a_page_that_needs_no_link_is_not_asked_again_tomorrow() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Bob cooks without peanuts when Carol eats here",
            "bob",
            "state",
            None,
        )
        .await;
        crate::planner::save_plan(&tree, &a_plan_of(&["casa", "intolleranze"], "bob"))
            .expect("save plan");

        // Both readings ask the same slot, so they are the same model.
        let judge = FakeLlmBackend::new("pro", "{\"verdicts\":{},\"links\":[]}");
        let report = judge_the_page(
            &pool,
            &tree,
            &judge,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;
        assert!(
            judge
                .last_prompt()
                .is_some_and(|p| p.contains("LEAVES A READER STUCK")),
            "the page was asked where it leads"
        );
        assert!(report.changed.is_empty(), "{:?}", report.changed);

        // The rail writer, with nothing skipped for having been read: the page
        // carries no link at all, which is its strongest nomination.
        let rails = FakeLlmBackend::new("pro", "{\"links\":[]}");
        let after = run_rail_writer(
            &pool,
            &tree,
            Some(&rails),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");

        assert_eq!(
            after.nominated, 1,
            "only the page that has not answered: {after:?}"
        );
        assert!(
            rails
                .last_prompt()
                .is_some_and(|p| p.contains("intolleranze —") && !p.contains("casa —")),
            "and the call bought was about the other page"
        );
        drop(dir);
    }

    /// **A page that loses a fact is a page saying something different.**
    ///
    /// «Nowhere» is remembered against what the page says, and what a fact says
    /// includes when it stopped being true. A page that answers «no link
    /// needed» and then has half its facts closed would otherwise hold that
    /// answer until somebody typed a new sentence on it.
    #[tokio::test]
    async fn closing_a_fact_reopens_the_question_of_where_the_page_leads() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Bob cooks without peanuts when Carol eats here",
            "bob",
            "state",
            None,
        )
        .await;
        let mut plan = a_plan_of(&["casa", "intolleranze"], "bob");
        crate::planner::save_plan(&tree, &plan).expect("save plan");

        let judge = FakeLlmBackend::new("pro", "{\"verdicts\":{},\"links\":[]}");
        judge_the_page(
            &pool,
            &tree,
            &judge,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        // The compile that follows the closure writes the end into the plan,
        // which is the same moment the page's own prose catches up.
        plan.pages
            .get_mut("casa")
            .expect("casa")
            .primary_facts
            .get_mut(0)
            .expect("a fact")
            .valid_to = Some("2026-09-14T00:00:00Z".to_owned());
        crate::planner::save_plan(&tree, &plan).expect("save plan again");

        let rails = FakeLlmBackend::new("pro", "{\"links\":[]}");
        let after = run_rail_writer(
            &pool,
            &tree,
            Some(&rails),
            "cycle-rails",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &BTreeSet::new(),
        )
        .await
        .expect("rail writer");

        // Both: the other page has never answered, and this one says something
        // different. The sibling above is the same fixture with the fact left
        // alone, and there the count is one.
        assert_eq!(
            after.nominated, 2,
            "the page is asked again, because it says something different: {after:?}"
        );
        drop(dir);
    }

    /// **A destination the page was not offered reaches nothing.**
    ///
    /// The list under the question is the whole of what may be answered: a
    /// model naming a page that is not on it has invented a neighbour, and a
    /// rail to an invented page is a mandatory sentence of prose pointing at
    /// nowhere. The choice is dropped and the page's reading stands.
    #[tokio::test]
    async fn a_link_to_a_page_the_judge_was_not_offered_parks_nothing() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        let fact = plant_on_page_of_kind(
            &tree,
            &pool,
            "bob",
            "casa.md",
            "Bob cooks without peanuts when Carol eats here",
            "bob",
            "state",
            None,
        )
        .await;
        crate::planner::save_plan(&tree, &a_plan_of(&["casa", "intolleranze"], "bob"))
            .expect("save plan");

        let llm = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{},\"links\":[{\"for\":\"f1\",\"to\":\"allergie\",\"why\":\"invented\"}]}",
        );
        let report = judge_the_page(
            &pool,
            &tree,
            &llm,
            std::slice::from_ref(&fact),
            JudgementDepth::Nightly,
            &RemPolicy::default(),
        )
        .await;

        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert!(
            report.refused[0].contains("was not offered"),
            "a dropped link is named, not swallowed: {:?}",
            report.refused
        );
        let saved = crate::planner::load_previous_plan(&tree)
            .expect("load")
            .expect("plan");
        assert!(
            saved.authored_rails.is_empty(),
            "{:?}",
            saved.authored_rails
        );
        drop(dir);
    }

    /// **What one reading answers, nobody buys a second call to ask.**
    ///
    /// This is the whole saving. The per-page split pass and the rail writer
    /// each carry a page to the same slot to ask it one question, and both are
    /// handed tonight's reading list: a page on it is not nominated, not
    /// rendered, not asked. What they keep is what the judge never sees — the
    /// page nobody wrote onto today.
    #[tokio::test]
    async fn the_passes_the_judge_absorbed_leave_its_pages_alone() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let planted = plant_on_page(&tree, &pool, "alice", "orto.md", 3, "alice").await;
        let judged: BTreeSet<&str> = std::iter::once("wikis/alice/orto.md").collect();

        // The splitter: the page is over the floor and would be nominated, and
        // the model splits whatever it is shown — so if the page reached it,
        // the page would come apart.
        let promote = FakeLlmBackend::new(
            "rp",
            format!(
                "{{\"split\": true, \"fact_ids\": [\"{}\"], \"target_page\": \"api.md\"}}",
                planted[0].as_str()
            ),
        );
        let report = run_auto_promote(
            &pool,
            &tree,
            Some(&promote),
            "cycle-skip",
            &day::DayPerimeter::default(),
            &mass_policy(),
            &load_smart_wiki_index(&tree).expect("index"),
            &judged,
        )
        .await
        .expect("auto promote");
        assert_eq!(
            report.candidates_examined, 0,
            "the page the judge reads is not carried to the splitter too: {report:?}"
        );
        assert!(report.applied.is_empty(), "{report:?}");

        // The rail writer: the page carries no link at all, which is the
        // strongest nomination it has.
        crate::planner::save_plan(&tree, &a_plan_of(&["orto", "compost"], "alice"))
            .expect("save plan");
        let rails = FakeLlmBackend::new("rp", "{\"links\":[]}");
        let report = run_rail_writer(
            &pool,
            &tree,
            Some(&rails),
            "cycle-skip",
            &day::DayPerimeter::default(),
            &RemPolicy::default(),
            &judged,
        )
        .await
        .expect("rail writer");
        assert_eq!(
            report.nominated, 1,
            "only the page the judge did not read: {report:?}"
        );
        assert!(
            rails
                .last_prompt()
                .is_some_and(|p| !p.contains("orto —") && p.contains("compost —")),
            "and the call that was bought was about that one"
        );
        drop(dir);
    }

    /// **What the day wrote is judged first.**
    ///
    /// *Is this fact on the right page?* is a question about a placement
    /// somebody just made. Asked in slug order — or in any order that ignores
    /// when the fact landed — the night's budget goes to the part of the
    /// corpus least likely to have moved, and the fact written this morning
    /// waits for a night with room. Two equally misfiled facts and one seat:
    /// the day's takes it.
    #[tokio::test]
    async fn the_refile_sweep_judges_what_the_day_wrote_first() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Alice loves pasta",
            "alice",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob plays the trumpet",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob repairs brass instruments",
            "bob",
            vec![0.0, 0.0, 1.0, 0.0],
        )
        .await;
        // Two facts filed in alice's wiki that both belong in bob's, on two
        // separate axes so neither anchors the other to alice. Only one seat,
        // so which of them is nominated is the whole assertion.
        let older = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Bob bought a new trumpet",
            "alice",
            vec![0.0, 0.0, 1.0, 0.0],
        )
        .await;
        let todays = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Bob plays in a brass band on Thursdays",
            "alice",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        // `older` is the newer row by insertion order, so without a perimeter
        // the newest-first tiebreak would take it — which is what makes this
        // test about the perimeter and not about insertion order.
        sqlx::query("UPDATE fact_index SET created_at = ? WHERE fact_id = ?")
            .bind("2026-08-30T00:00:00Z")
            .bind(older.as_str())
            .execute(&pool)
            .await
            .expect("age the older fact");
        sqlx::query("UPDATE fact_index SET created_at = ? WHERE fact_id = ?")
            .bind("2026-08-20T00:00:00Z")
            .bind(todays.as_str())
            .execute(&pool)
            .await
            .expect("age today's fact");

        let day = day::DayPerimeter {
            since: Some(
                chrono::DateTime::parse_from_rfc3339("2026-08-19T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            facts_written: std::iter::once(todays.as_str().to_owned()).collect(),
            ..day::DayPerimeter::default()
        };

        let resp = "{\"verdict\":\"stay\",\"reason\":\"home is fine\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let policy = RemPolicy {
            refile_sweep_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_refile_sweep(&pool, &tree, &llm, "cycle-day", &day, &policy, &index)
            .await
            .expect("sweep");

        assert_eq!(report.candidates_examined, 1, "one seat: {report:?}");
        let shown = format!(
            "{}\n{}",
            llm.last_system_prompt().unwrap_or_default(),
            llm.last_prompt().expect("the confirmer was asked")
        );
        assert!(
            shown.contains("brass band"),
            "the day's fact took the seat: {shown}"
        );
        assert!(
            !shown.contains("bought a new trumpet"),
            "and the older one waits its turn: {shown}"
        );
        drop(dir);
    }

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

        // The model names the page too, and it must be one bob ALREADY has
        // (`preferenze.md`, where `plant_fact_with_embedding` files): a name
        // bob does not hold would mint a second plan key under the bare-slug
        // keyspace and strand `wiki_id != source_path`, so the engine
        // refuses it and the fact stays home.
        let resp = "{\"verdict\":\"move\",\"dest_wiki_id\":\"bob\",\"dest_page\":\"preferenze.md\",\"reason\":\"this fact is about bob\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            &day::DayPerimeter::default(),
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
        assert_eq!(row.source_path, "wikis/bob/preferenze.md");
        assert!(row.deleted_at.is_none(), "refile is never a tombstone");

        // Born-applied receipt, and nobody told.
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
        assert_eq!(notices, 0, "the nightly cycle notices nobody");
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

        let resp = "{\"verdict\":\"move\",\"dest_wiki_id\":\"bob\",\"dest_page\":\"preferenze.md\",\"reason\":\"the subject is bob\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-bridge",
            &day::DayPerimeter::default(),
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
            &day::DayPerimeter::default(),
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

    /// A `@rules.md` fact is never NOMINATED for refile, however foreign it
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
        // themselves). The behaviour rule lives on alice's `@rules.md` and
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
            &day::DayPerimeter::default(),
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
        assert_eq!(row.source_path, "wikis/alice/@rules.md");
        drop(dir);
    }

    /// The revisor never pairs a `@rules.md` fact with a non-rules fact
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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

    /// 🚨 The audience gate. Two facts saying the same thing to **different
    /// people** are two facts: merging them would retire one reader's memory
    /// and hand the survivor to the other's readers. The pair must die before
    /// the confirmer ever sees it — a rule the model could weigh is a rule
    /// that fails on the day it matters.
    #[tokio::test]
    async fn revisor_refuses_a_pair_whose_readers_differ() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Same subject, same wording family — and one of them is also
        // readable by Carol. That extra reader is the whole difference.
        let shared = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "bob prefers tea with milk every morning",
            "bob",
            vec![Principal::User("carol".to_owned())],
            None,
        )
        .await;
        let private = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "bob likes morning tea with a splash of milk",
            "bob",
            Vec::new(),
            None,
        )
        .await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
            .await
            .unwrap();

        assert_eq!(
            report.revisor.pairs_examined, 0,
            "a pair with two audiences must never reach the confirmer: {report:?}"
        );
        assert!(report.revisor.applied.is_empty());
        for fid in [&shared, &private] {
            let row = fact_index::find_by_id(&pool, fid)
                .await
                .unwrap()
                .expect("row");
            assert!(
                row.superseded_at.is_none(),
                "neither side may be retired: {fid}"
            );
        }
        drop(dir);
    }

    /// The gate is a gate, not a wall: the same two claims held for the same
    /// readers still consolidate. Otherwise 54a would have bought governance
    /// by switching dedup off.
    #[tokio::test]
    async fn revisor_still_pairs_two_facts_with_the_same_readers() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let carol = || vec![Principal::User("carol".to_owned())];
        let old_id = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "bob prefers tea with milk every morning",
            "bob",
            carol(),
            None,
        )
        .await;
        let _new_id = plant_fact_with_acl(
            &tree,
            &pool,
            "bob",
            "bob likes morning tea with a splash of milk",
            "bob",
            carol(),
            None,
        )
        .await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
            .await
            .unwrap();

        assert!(
            report.revisor.pairs_examined > 0,
            "one audience, one fact — the pair must still be nominated: {report:?}"
        );
        let row = fact_index::find_by_id(&pool, &old_id)
            .await
            .unwrap()
            .expect("row");
        assert!(
            row.superseded_at.is_some(),
            "the older side retires as it always did: {report:?}"
        );
        drop(dir);
    }

    /// Rule-vs-rule pairs stay nominable: two near-duplicate directives on
    /// the same `@rules.md` page still reach the confirmer and merge.
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
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(&pool, &tree, fake_embedder(), &test_llms(&rev_llm), &policy)
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
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
            &BTreeSet::new(),
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

    /// **A contradiction wholly on a page tonight's reading will open is left
    /// to that reading.**
    ///
    /// The reading has the whole page in front of it and answers
    /// `contradicted` with both facts in view, so sending the pair here as
    /// well carries one page to a model twice. The sibling above is the same
    /// fixture with nothing selected, and there the sweep does close it — which
    /// is what makes this a fence and not an accident.
    #[tokio::test]
    async fn a_contradiction_wholly_on_a_page_the_reading_will_open_is_left_to_it() {
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
            .await
            .expect("supersede");

        let where_it_lives = fact_index::find_by_id(&pool, &departure)
            .await
            .unwrap()
            .expect("seed")
            .source_path;
        assert_eq!(
            fact_index::find_by_id(&pool, &satellite)
                .await
                .unwrap()
                .expect("satellite")
                .source_path,
            where_it_lives,
            "the fixture puts the pair on one page, which is the whole case"
        );
        let will_be_read: BTreeSet<&str> = std::iter::once(where_it_lives.as_str()).collect();

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
            &will_be_read,
        )
        .await
        .expect("sweep");

        assert_eq!(report.seeds_examined, 0, "{report:?}");
        assert!(report.closed.is_empty(), "{report:?}");
        assert!(
            llm.last_prompt().is_none(),
            "and no call was bought for a question the reading already carries"
        );
        assert!(
            fact_index::find_by_id(&pool, &satellite)
                .await
                .unwrap()
                .expect("satellite")
                .valid_to
                .is_none(),
            "the satellite is untouched, waiting for the reading"
        );
        drop(dir);
    }

    /// The cancelled dated event, end to end — the case the sub-job was
    /// written for and could not reach.
    ///
    /// Both the seed and its satellite carry a **future** `valid_to`, which is
    /// what `ingest.md` tells the classifier to give a dated commitment. Two
    /// things make this reachable: the candidate pool does not require an open
    /// horizon (requiring `valid_to IS NULL` would make it disjoint from the
    /// due-soon slot's `valid_to IS NOT NULL`, so the sweep would never see the
    /// facts that keep firing), and the closure refuses a horizon still ahead
    /// of it (`mark_superseded` COALESCEs, so a dated seed keeps its own date,
    /// and stamping the satellite with it would file the satellite straight
    /// back into the due-soon window).
    #[tokio::test]
    async fn contradiction_sweep_closes_a_dated_satellite_before_its_own_date() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let future = (Utc::now() + chrono::Duration::days(30)).to_rfc3339();
        let dated = async |body: &str| {
            plant_fact_with_window(
                &tree,
                &pool,
                "alice",
                body,
                "alice",
                None,
                Some(future.clone()),
            )
            .await
        };
        let departure = dated("Partenza per Parigi il 15 giugno").await;
        let satellite = dated("Itinerario giorno 1: Louvre").await;
        let cancellation = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il viaggio a Parigi è annullato",
            "alice",
        )
        .await;
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
            .await
            .expect("supersede");
        // The seed kept its own future horizon — COALESCE, not overwrite.
        let seed = fact_index::find_by_id(&pool, &departure)
            .await
            .unwrap()
            .expect("seed");
        assert_eq!(
            seed.valid_to.as_deref(),
            Some(future.as_str()),
            "the seed's original date survives the supersede"
        );

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
            &BTreeSet::new(),
        )
        .await
        .expect("sweep");

        assert_eq!(
            report.closed,
            vec![satellite.as_str().to_owned()],
            "a dated satellite can be nominated at all"
        );
        let row = fact_index::find_by_id(&pool, &satellite)
            .await
            .unwrap()
            .expect("row");
        let closed_at = row.valid_to.as_deref().expect("the satellite fell");
        assert_ne!(
            closed_at, future,
            "and it does not inherit the trip's own date — that would put it \
             back in the due-soon slot as an imminent commitment"
        );
        assert!(
            DateTime::parse_from_rfc3339(closed_at)
                .expect("rfc3339")
                .to_utc()
                <= Utc::now(),
            "it fell when the trip was cancelled, which is in the past"
        );
        drop(dir);
    }

    /// A satellite falls when the SEED fell — the instant the seed's own
    /// `valid_to` already carries — not when the sweep noticed it had.
    ///
    /// `superseded_at` dates the cycle that read the contradiction. It is the
    /// same instant on a live turn and two months later on a backlog replay,
    /// where it stamps a satellite with tonight's date and files a June
    /// cancellation under August.
    #[tokio::test]
    async fn a_satellite_falls_when_the_seed_fell_not_when_the_sweep_noticed() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let fell = (Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        let departure = plant_fact_with_window(
            &tree,
            &pool,
            "alice",
            "Partenza per Parigi il 15 giugno",
            "alice",
            None,
            Some(fell.clone()),
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
            .await
            .expect("supersede");
        let seed = fact_index::find_by_id(&pool, &departure)
            .await
            .unwrap()
            .expect("seed");
        assert_eq!(
            seed.valid_to.as_deref(),
            Some(fell.as_str()),
            "COALESCE, not overwrite: the seed keeps the instant it stopped \
             being true"
        );
        assert_ne!(
            seed.superseded_at.as_deref(),
            Some(fell.as_str()),
            "and the sweep read it two months later — the two clocks are \
             visibly apart, which is what makes this test able to fail"
        );

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
            &BTreeSet::new(),
        )
        .await
        .expect("sweep");

        assert_eq!(report.closed, vec![satellite.as_str().to_owned()]);
        let row = fact_index::find_by_id(&pool, &satellite)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.valid_to.as_deref(),
            Some(fell.as_str()),
            "the satellite fell with the seed, at the seed's own instant"
        );
        drop(dir);
    }

    /// Rules-page facts are never satellite candidates: a standing
    /// directive leaves the channel only via supersede / tombstone / its
    /// subject's explicit closure — never as collateral of a neighbouring
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
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("@rules.md")),
                body: "Rispondi sempre anche a voce.".to_owned(),
                subject: Principal::User("alice".to_owned()),
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
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
            &BTreeSet::new(),
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
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
            &BTreeSet::new(),
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
        fact_index::mark_superseded(&pool, &v1, &v2, chrono::Utc::now())
            .await
            .expect("supersede v1");
        fact_index::mark_superseded(&pool, &v2, &v3, chrono::Utc::now())
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
            &BTreeSet::new(),
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
        fact_index::mark_superseded(&pool, &departure, &cancellation, chrono::Utc::now())
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
            &BTreeSet::new(),
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
        subject: &str,
        topics: &[&str],
    ) -> FactId {
        let req = crate::capture::CaptureRequest {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: Principal::User(subject.to_owned()),
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
        // card match the turn's seed ("ricette") for the gather fan. On
        // `dolci.md`, which is therefore a page `ricette` already holds — the
        // only kind of page a cross-wiki refile may name.
        plant_topic_fact(
            &tree,
            &pool,
            "ricette",
            "dolci.md",
            "Le ricette di famiglia sono raccolte qui",
            "alice",
            &["ricette"],
        )
        .await;
        crate::recall_log::record_miss(
            &pool,
            &crate::recall_log::NewMiss {
                created_at: &chrono::Utc::now().to_rfc3339(),
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
            "{\"verdict\":\"move\",\"dest_wiki_id\":\"ricette\",\"dest_page\":\"dolci.md\",\"reason\":\"è una ricetta\"}",
        );
        let navigator = FakeLlmBackend::new(
            "nav",
            "{\"open\":[{\"wiki_id\":\"ricette\",\"page\":\"dolci.md\"}],\"done\":true}",
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
            moved.source_path, "wikis/ricette/dolci.md",
            "the fact landed on the page the judge named, which `ricette` already had"
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
        assert_eq!(status, "applied", "born-applied receipt");
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
                    created_at: &(chrono::Utc::now() + chrono::Duration::minutes(i64::from(i)))
                        .to_rfc3339(),
                    sender_id: "alice",
                    fact_id: target.as_str(),
                    wiki_id: "alice",
                    source_path: "wikis/alice/preferenze.md",
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
        assert_eq!(
            notices, 1,
            "a recall miss the engine cannot repair is the OPERATOR's problem, \
             and that notice stays"
        );
        // And it has a home an operator can actually reach. The event comes
        // out of `events_poll`, which hands a notice addressed to nobody only
        // to the operator's own token; an operator who works the dashboard
        // would otherwise never meet this at all.
        let rows = crate::proposals::list(&pool, &crate::proposals::ListFilters::default())
            .await
            .expect("list");
        let row = rows
            .iter()
            .find(|r| r.kind == crate::proposals::kind::RECALL_TUNING)
            .expect("the notice has a row on the proposals page");
        // Born applied: nothing here is anybody's to apply, so it never joins
        // the queue of things waiting on a decision. Addressed to nobody:
        // it is about the engine's own recall rather than about anybody's
        // facts, which is what puts it in front of the administrator without
        // the reveal lens.
        assert_eq!(row.status, crate::proposals::ProposalStatus::Applied);
        assert!(
            row.recipient_id.is_none(),
            "an engine notice is addressed to nobody, not to a person"
        );
        assert_eq!(row.context["miss_count"], serde_json::json!(3));
        assert!(
            row.context.get("sample_query").is_none(),
            "the sentence the person asked stays in `recall_log`, not on a row a page renders"
        );
        assert!(
            !row.context.to_string().contains("codice del cancello"),
            "nothing quoting the person reaches the row: {}",
            row.context
        );
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

    /// A BACKFILLED fact resolves "oggi" against the day it was uttered —
    /// `valid_from`, the stored projection of the turn's `occurred_at` clock —
    /// and not against the wall-clock day its row was inserted.
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

    /// The other side of [`fact_began`], and the one the code got wrong: a
    /// fact whose `valid_from` is a real FUTURE start («da luglio lavoro a
    /// Milano», said in April) anchors on `created_at`.
    ///
    /// `valid_from` is the start of HOLDING, not the moment of speaking.
    /// Reading it as the anchor resolves the sentence's "oggi" against a day
    /// that has not happened yet — so the earlier of the two clocks wins, and
    /// every pass that dates a fact is protected by this, not only the
    /// normalizer.
    #[tokio::test]
    async fn date_normalizer_refuses_an_anchor_that_has_not_happened_yet() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let dated = plant_fact(&tree, &pool, "alice", "Oggi ha giocato 31 minuti", "alice").await;
        let future = (Utc::now() + chrono::Duration::days(90)).to_rfc3339();
        sqlx::query("UPDATE fact_index SET valid_from = ? WHERE fact_id = ?")
            .bind(&future)
            .bind(dated.as_str())
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
            !prompt.contains(future.as_str()),
            "a day that has not happened cannot be what «oggi» meant: {prompt}"
        );
        let row = fact_index::find_by_id(&pool, &dated)
            .await
            .unwrap()
            .expect("row");
        assert!(
            prompt.contains(row.created_at.as_str()),
            "the write instant is the earlier of the two, so it is the anchor: {prompt}"
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

    /// The detector anchors on one exact trailing shape and nothing else:
    /// mid-prose links, prose-bearing parentheticals, glued suffixes, and
    /// slash-less targets are content and never match.
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
        write_smart_wiki(&tree, "alice-lnprint", "lnprint smart wiki", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(
            &tree,
            &pool,
            "alice-lnprint",
            "Design note. ([[alice-lnprint/appunti]])",
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

    // ---------- consolidation scopes ----------

    /// Every standard wiki is one scope, and a smart wiki is none.
    #[tokio::test]
    async fn every_standard_wiki_is_its_own_consolidation_scope() {
        let (dir, mut tree, _pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        write_wiki(&tree, "famiglia-amici", "Amici", "wiki-group");
        write_smart_wiki(&tree, "alice-lnprint", "lnprint smart wiki", "alice");
        tree = WikiTree::open(dir.path()).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let mut got: Vec<String> = consolidation_scopes(&tree, &index)
            .expect("scopes")
            .into_iter()
            .map(|s| s.wiki_id)
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec!["famiglia".to_owned(), "famiglia-amici".to_owned()],
            "one scope per standard wiki, and the smart one is out",
        );
        drop(dir);
    }

    /// The confirmer sweeps look the rubric up by the wiki a case came from,
    /// so an agent's own memory must answer "yes" and nobody else's may.
    #[tokio::test]
    async fn agent_wikis_flags_the_agents_own_memory_only() {
        let (dir, mut tree, _pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "hermes1", "Hermes", "wiki-user");
        let meta_path = tree.wikis_dir().join("hermes1").join("_meta.md");
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        std::fs::write(
            &meta_path,
            raw.replace("---\nwiki_id:", "---\nis_agent: true\nwiki_id:"),
        )
        .unwrap();
        tree = WikiTree::open(dir.path()).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let map = agent_wikis(&consolidation_scopes(&tree, &index).expect("scopes"));
        assert_eq!(map.get("hermes1"), Some(&true));
        assert_eq!(map.get("alice"), Some(&false));
    }

    /// The dedup rubric changes inside an agent's own memory: there, WHO an
    /// episode was lived with is part of the fact, so two near-identical
    /// sentences about two different people are two memories. The scope
    /// resolves the marker once instead of re-locating a wiki per candidate
    /// pair.
    #[tokio::test]
    async fn an_agents_wiki_carries_the_autobiography_dedup_rubric() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "hermes1", "Hermes", "wiki-user");
        // Stamp the agent marker on the bot's wiki only.
        let meta_path = tree.wikis_dir().join("hermes1").join("_meta.md");
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        std::fs::write(
            &meta_path,
            raw.replace("---\nwiki_id:", "---\nis_agent: true\nwiki_id:"),
        )
        .unwrap();
        tree = WikiTree::open(dir.path()).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let scopes = consolidation_scopes(&tree, &index).expect("scopes");
        for s in &scopes {
            assert_eq!(
                s.is_agent,
                s.wiki_id == "hermes1",
                "only the agent's own memory is flagged; got {:?}",
                s.wiki_id
            );
        }

        // Two episodes of the same shape lived with two different people.
        plant_fact_on_page(
            &tree,
            &pool,
            "hermes1",
            "preferenze.md",
            "Ho aiutato Alice con la pratica INPS.",
            "hermes1",
        )
        .await;
        plant_fact_on_page(
            &tree,
            &pool,
            "hermes1",
            "preferenze.md",
            "Ho aiutato Bob con la pratica INPS.",
            "hermes1",
        )
        .await;
        let rows = fact_index::find_active_in_wiki(&pool, "hermes1")
            .await
            .expect("rows");
        let (new, old) = (&rows[1], &rows[0]);
        let with = revisor_prompt(&tree, new, old, true).expect("agent prompt");
        assert!(
            with.contains("AGENT AUTOBIOGRAPHY"),
            "the agent rubric must reach the confirmer:\n{with}"
        );
        let without = revisor_prompt(&tree, new, old, false).expect("plain prompt");
        assert!(
            !without.contains("AGENT AUTOBIOGRAPHY"),
            "a human's family keeps the ordinary rubric:\n{without}"
        );
        drop(dir);
    }

    /// A backend that fails the first `fail_first` calls and then answers.
    /// The shape of the live 2026-07-29 incident: one malformed Gemini
    /// candidate in the middle of an otherwise healthy night. Shared by
    /// every sub-job's failure test, because they all read one rule.
    struct FlakyLlm {
        fail_first: std::sync::atomic::AtomicUsize,
        answer: String,
    }

    #[async_trait::async_trait]
    impl LlmBackend for FlakyLlm {
        fn model_id(&self) -> &'static str {
            "flaky"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::llm::Result<crate::llm::CompletionResponse> {
            if self
                .fail_first
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| (n > 0).then_some(n - 1),
                )
                .is_ok()
            {
                return Err(crate::llm::LlmError::Protocol(
                    "gemini response has no `text` part in the first candidate".to_owned(),
                ));
            }
            Ok(crate::llm::CompletionResponse {
                text: self.answer.clone(),
                finish_reason: crate::llm::FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage::default(),
            })
        }
    }

    /// One bad response must cost one pair, not the night. A revisor that
    /// propagated the first LLM error would abort `dream::run_full` on it,
    /// taking the promote, the reorg and every queued page compile with it,
    /// retry a day away.
    #[tokio::test]
    async fn revisor_skips_a_pair_the_backend_fumbled_and_keeps_going() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let older = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "preferenze.md",
            "Carol è la sorella di Franz e vive a Bologna",
            "alice",
        )
        .await;
        let _newer = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "preferenze.md",
            "Carol è la sorella di Franz e vive a Bologna in centro",
            "alice",
        )
        .await;

        let llm = FlakyLlm {
            fail_first: std::sync::atomic::AtomicUsize::new(1),
            answer: "{\"same\": true}".to_owned(),
        };
        let report = run_revisor_jaccard(
            &pool,
            &tree,
            &fake_embedder(),
            &llm,
            "cycle-flaky",
            &RemPolicy::default(),
            &load_smart_wiki_index(&tree).expect("index"),
        )
        .await
        .expect("a fumbled pair is a soft error, never a cycle abort");
        assert_eq!(report.errors.len(), 1, "the skip is recorded: {report:?}");
        assert!(report.applied.is_empty(), "nothing merged: {report:?}");
        // The pair stays nominable: no negative verdict was memoised for it.
        let survivor = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .expect("older row still there");
        assert!(survivor.superseded_at.is_none());
        drop(dir);
    }

    /// …but a backend that is simply down still stops the cycle: nothing
    /// downstream would work either, and pretending otherwise would bury
    /// the outage in a soft-error list nobody reads.
    #[tokio::test]
    async fn revisor_aborts_when_the_backend_keeps_failing() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Five near-variants of one claim: every pair lands inside the
        // jaccard nomination band, so the sweep has more than
        // `LLM_FAILURE_ABORT` confirms to attempt.
        for tail in [
            "in centro",
            "da molti anni",
            "con la moglie",
            "vicino al parco",
            "dal 1990",
        ] {
            plant_fact_on_page(
                &tree,
                &pool,
                "alice",
                "preferenze.md",
                &format!("Carol è la sorella di Franz e vive a Bologna {tail}"),
                "alice",
            )
            .await;
        }
        let llm = FlakyLlm {
            fail_first: std::sync::atomic::AtomicUsize::new(usize::MAX),
            answer: String::new(),
        };
        let err = run_revisor_jaccard(
            &pool,
            &tree,
            &fake_embedder(),
            &llm,
            "cycle-down",
            &RemPolicy::default(),
            &load_smart_wiki_index(&tree).expect("index"),
        )
        .await
        .expect_err("a dead backend must surface");
        assert!(
            format!("{err}").contains("consecutive revisor failures"),
            "the diagnostic must name the outage: {err}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn revisor_dedups_two_pages_of_one_wiki() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        tree = WikiTree::open(dir.path()).unwrap();
        // The split-subject duplicate: the same identity fact captured twice,
        // on two pages of the wiki. Jaccard-kin but not identical (the
        // capture-time dedup threshold is off).
        let older = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "carol.md",
            "Carol è la sorella di Franz e vive a Bologna",
            "alice",
        )
        .await;
        let newer = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "anagrafica.md",
            "Carol è la sorella di Franz e vive a Bologna in centro",
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
        assert_eq!(report.applied.len(), 1, "the pair merged: {report:?}");
        let loser = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .unwrap();
        assert!(loser.superseded_at.is_some(), "the older copy retired");
        assert_eq!(loser.superseded_by.as_ref(), Some(&newer));
        let winner = fact_index::find_by_id(&pool, &newer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            winner.wiki_id, "famiglia",
            "the survivor stays where it lives"
        );
        drop(dir);
    }

    /// Identity-core stickiness (leva 3): the REM dedup revisor never
    /// retires a fact from a person's always-on identity core
    /// (`bio` + `salience=high`), even a genuine near-duplicate. A
    /// relationship like "X è il compagno di Y" changes only on an
    /// explicit correction, never by silent background consolidation.
    /// Same near-duplicate pair as `revisor_dedups_two_pages_of_one_wiki`,
    /// but the would-be loser is identity-core — so nothing merges.
    #[tokio::test]
    async fn revisor_never_retires_an_identity_core_fact() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        tree = WikiTree::open(dir.path()).unwrap();
        let older = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "carol.md",
            "Carol è la sorella di Franz e vive a Bologna",
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
            "famiglia",
            "anagrafica.md",
            "Carol è la sorella di Franz e vive a Bologna in centro",
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
        let old_text = "È nato il 22 settembre 1968";
        let new_text = "Frodo Baggins, hobbit della Contea (Frodo) è nato il 22 settembre 1968.";
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
            "preferenze.md",
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
            "preferenze.md",
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
        assert!(
            prompt.contains("franz · wikis/franz/preferenze.md"),
            "{prompt}"
        );
        drop(dir);
    }

    // ---------- husk-page GC ----------

    /// Supersede `fact_id`, so its region is a marker rather than content.
    async fn supersede(pool: &SqlitePool, fact_id: &FactId) {
        let succ = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dff").unwrap();
        fact_index::mark_superseded(pool, fact_id, &succ, chrono::Utc::now())
            .await
            .expect("supersede");
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
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(tree, &plan).unwrap();
    }

    /// The husk shape end-to-end: a plan-absent page whose only row is
    /// superseded is removed (offsets settled); the per-cycle cap defers
    /// the rest in deterministic path order; a missing plan is a no-op.
    #[tokio::test]
    async fn husk_gc_removes_plan_absent_pages_whose_rows_are_all_superseded() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let f1 = plant_fact_on_page(&tree, &pool, "alice", "vecchia.md", "husk uno", "alice").await;
        let f2 = plant_fact_on_page(&tree, &pool, "alice", "vetusta.md", "husk due", "alice").await;
        supersede(&pool, &f1).await;
        supersede(&pool, &f2).await;
        let index = load_smart_wiki_index(&tree).expect("index");

        // No plan on disk → no-op (unplanned ≠ husk).
        let report = run_husk_gc(&pool, &tree, "cycle-husk", &RemPolicy::default(), &index)
            .await
            .expect("sweep");
        assert_eq!(report.pages_examined, 0, "no plan → nothing examined");
        assert!(report.removed.is_empty());

        save_unrelated_plan(&tree);
        let capped = RemPolicy {
            husk_gc_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_husk_gc(&pool, &tree, "cycle-husk", &capped, &index)
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
        // retirement sweep never reopens a file that has been removed.
        let row = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert!(row.region_start.is_none() && row.region_end.is_none());

        // Next cycle drains the backlog.
        let report = run_husk_gc(&pool, &tree, "cycle-husk-2", &RemPolicy::default(), &index)
            .await
            .expect("sweep");
        assert_eq!(report.removed, vec!["wikis/alice/vetusta.md".to_owned()]);
        assert!(!dir.path().join("wikis/alice/vetusta.md").exists());
        drop(dir);
    }

    /// The DB-first guards: an active row keeps the file, plan membership
    /// keeps the file (never examined), and reserved names never qualify.
    /// A supersession does NOT keep it: a superseded row leaves only a
    /// marker, and a marker is not content.
    #[tokio::test]
    async fn husk_gc_keeps_active_planned_and_reserved_pages_but_not_a_freshly_superseded_one() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        // Active fact → blocks.
        plant_fact_on_page(&tree, &pool, "alice", "attiva.md", "fatto vivo", "alice").await;
        // Superseded a moment ago → does NOT block.
        let fresh =
            plant_fact_on_page(&tree, &pool, "alice", "fresca.md", "appena caduto", "alice").await;
        let succ = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dfe").unwrap();
        fact_index::mark_superseded(&pool, &fresh, &succ, chrono::Utc::now())
            .await
            .expect("supersede");
        // Plan-member page with no rows → never a candidate.
        std::fs::write(dir.path().join("wikis/alice/pianificata.md"), "# planned\n").unwrap();
        // Reserved name with no rows → never a candidate.
        std::fs::write(dir.path().join("wikis/alice/@rules.md"), "# rules\n").unwrap();

        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "pianificata".to_owned(),
            PagePlan {
                slug: "pianificata".to_owned(),
                title: "Pianificata".to_owned(),
                description: String::new(),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
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
            authored_rails: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_husk_gc(&pool, &tree, "cycle-husk", &RemPolicy::default(), &index)
            .await
            .expect("sweep");
        assert_eq!(
            report.pages_examined, 2,
            "only the two plan-absent, non-reserved pages are checked"
        );
        assert_eq!(
            report.removed,
            vec!["wikis/alice/fresca.md".to_owned()],
            "the freshly superseded husk goes; nothing else does: {report:?}"
        );
        assert!(
            !dir.path().join("wikis/alice/fresca.md").exists(),
            "a superseded row leaves only a marker"
        );
        for page in ["attiva.md", "pianificata.md", "@rules.md"] {
            assert!(
                dir.path().join("wikis/alice").join(page).exists(),
                "{page} must survive"
            );
        }
        drop(dir);
    }

    #[test]
    fn the_recall_tuning_notice_names_the_fact_and_never_quotes_the_person() {
        let fact_id = FactId::parse("018f1234-5678-7abc-9def-0123456789c1").unwrap();
        let fact = FactIndexRow {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: "bob".to_owned(),
            source_path: "wikis/bob/casa.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "The gate code is 4417.".to_owned(),
            embedding: vec![0.1, 0.2],
            subject_id: "user:bob".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("episode".to_owned()),
            topics: Vec::new(),
            created_at: "2026-03-14T18:00:00Z".to_owned(),
            updated_at: "2026-03-14T18:00:00Z".to_owned(),
            superseded_at: None,
            superseded_by: None,
            successor_fact_id: None,
            deleted_at: None,
            deleted_reason: None,
            last_recall_at: None,
            recall_count_30d: 0,
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        };
        let payload =
            recall_tuning_payload(&fact, fact_id.as_str(), 4, Some("no candidate passed"));

        // The operator's evidence: which fact, where it lives, how often.
        assert_eq!(payload["fact_id"], json!(fact_id.as_str()));
        assert_eq!(payload["source_path"], json!("wikis/bob/casa.md"));
        assert_eq!(payload["miss_count"], json!(4));
        assert_eq!(payload["gate"], json!("no candidate passed"));
        // And nothing of what the person said. The notice travels — over MCP,
        // into a consumer's context, through whatever model it runs — and the
        // restated query is the person's own sentence.
        assert!(
            payload.get("sample_query").is_none(),
            "the restated query must not ride the notice: it stays in `recall_log`"
        );
        let whole = payload.to_string();
        assert!(
            !whole.contains("gate code"),
            "nothing quoting the person may reach the payload: {whole}"
        );
    }
}
