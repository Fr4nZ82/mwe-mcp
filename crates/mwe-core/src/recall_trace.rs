// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded journal of recall runs — the story behind the admin Traces page.
//!
//! A "recall trace" records the **whole route** one recall took: the flat /
//! fresh / due-soon hits, the entry-point fan, every navigator hop (the
//! candidates offered, the decision with its one-line note, the pages that
//! actually opened) and the block that was finally injected into the
//! consumer. Two producers write here:
//!
//! - the **ingest per-turn injection** ([`crate::ingest::wiki_ingest_message`])
//!   — what a bridge-connected consumer received for a user's turn;
//! - the **`wiki_navigate` tool** — what an explicitly-searching consumer got
//!   back from deep recall.
//!
//! The journal is **age-pruned**, like its two siblings in
//! [`crate::recall_log`]: [`record_trace`] deletes rows past the retention
//! window after every insert, and the window is an operator setting
//! (`recall.trace_retention_days`, default
//! [`DEFAULT_TRACE_RETENTION_DAYS`]). Recording is best-effort telemetry: a
//! journal write must never fail or mask the recall itself, so callers
//! log-and-ignore the error.
//!
//! **Why a window and not a handful of rows.** Keeping the newest ten rows
//! deployment-wide — on production's ~30 turns a day — discards better than
//! 99 % of this within the hour. What that throws away is the only labelled
//! record the engine ever produces of *how recall behaved*: the
//! candidates offered per hop, the navigator's own one-line reason for each
//! choice, and what it then opened. That is the evidence base the rewiring
//! pass needs — a page repeatedly offered and declined has a card that
//! misdescribes it — and a pass fed ten rows can conclude nothing. Sized
//! against the sibling tables so the whole recall-telemetry family ages out on
//! one comprehensible schedule.
//!
//! The payload is versioned JSON ([`RecallTrace`], [`TRACE_PAYLOAD_VERSION`])
//! stored in the `recall_traces` table (migration `0057`); every field is
//! `serde(default)` so a reader tolerates rows written by an older shape. With
//! a retention window measured in months, rows of the previous shape are on
//! screen for as long as the window lasts: a field added here must render
//! sensibly when it is absent, and [`RecallTrace::version`] is what tells a
//! surface which absence it is looking at.
//!
//! **A trace is scoped to the sender it was recorded for**, and the dashboard
//! surface enforces that per row — your own always, anybody else's only under
//! the admin reveal switch. Scope the *query*, not the fetched page
//! ([`recent_traces_for_sender`]): filtering a deployment-wide page in the
//! handler would show a user fewer of their own traces the busier the
//! deployment gets.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::recall::RecallHit;
use crate::recall_nav::{EntryPoint, HopTrace};

/// Default retention of the trace journal, in days.
///
/// Matches `recall_misses` — the repair queue these traces are the evidence
/// for — rather than the leaner 30-day `recall_log`, because a trace is read
/// long after its turn: it is the raw material a rewiring pass and a growing
/// gold set are distilled from. Overridable per deployment with
/// `recall.trace_retention_days`; a trace holds the recall block verbatim, so
/// an operator who wants a shorter clear-text window sets it here.
pub const DEFAULT_TRACE_RETENTION_DAYS: i64 = 90;

/// Payload schema version written by this build.
///
/// A reader tolerates every version — every field is `serde(default)` — so
/// the number is not a gate, it is what tells a surface whether a field is
/// absent because the turn had nothing to put there or because the row
/// predates the field:
///
/// - **1** — the first shape: the hits, the fan, the hops, the injected
///   block and the wall clock of the turn.
/// - **2** — what the recall actually did. The producer and the turn's
///   requested depth; the classifier's completed message and whether the
///   flat hits answer it; per hit its kind, the seat it took, the link-key
///   win and what the flat slot then did with it; the pages the identity
///   slot served whole; the project-documentation slot; the fact that opened
///   each `rag` door; the vetting refusal behind a pick that did not open;
///   and a recall-only figure beside the whole-turn one.
/// - **3** — the reconciliation stage: the candidates it was shown and the
///   answer it gave, verbatim. It is the one call in the turn that can retire
///   a stored fact, and until this it was the one call that left no record of
///   what it was asked or what it said.
pub const TRACE_PAYLOAD_VERSION: u32 = 3;

/// Byte cap on the journaled turn / query text.
const TURN_TEXT_CAP: usize = 1_200;
/// Byte cap on each journaled hit body.
const HIT_TEXT_CAP: usize = 500;
/// Byte cap on each journaled reconciliation candidate.
///
/// Shorter than a hit's: this list exists to say WHICH facts were put to the
/// stage, and a turn that read a card puts every fact on it there — so the
/// list is long and each line only has to be recognisable.
const RECONCILE_TEXT_CAP: usize = 200;

/// Which surface produced a trace. Stored as [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceSource {
    /// The ingest per-turn injection (`wiki_ingest_message`).
    Ingest,
    /// The `wiki_navigate` deep-recall tool.
    Navigate,
}

impl TraceSource {
    /// The DB token (`'ingest'` | `'navigate'`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Navigate => "navigate",
        }
    }

    /// Parse a stored token. Unknown values fall back to [`Self::Ingest`] —
    /// we control every write, so this only keeps the read path total.
    #[must_use]
    fn from_db(s: &str) -> Self {
        match s {
            "navigate" => Self::Navigate,
            _ => Self::Ingest,
        }
    }
}

/// The versioned trace payload — one recall run, whole route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallTrace {
    /// Payload schema version ([`TRACE_PAYLOAD_VERSION`]).
    pub version: u32,
    /// Which surface wrote this row ([`TraceSource::as_str`]): `ingest` for
    /// a conversational turn, `navigate` for the `wiki_navigate` tool.
    ///
    /// The two are different animals and half the fields below mean
    /// different things in each, so the payload names its own producer
    /// rather than depending on the row it happens to be stored in.
    pub producer: String,
    /// Consumer that carried the call, when the transport knew one — the
    /// bridge on an `ingest` turn, the token's claim on `wiki_navigate`.
    /// Empty when the caller sent none.
    pub consumer: Option<String>,
    /// The turn / query text (≤ [`TURN_TEXT_CAP`] bytes).
    pub turn_text: String,
    /// The classifier's reading of the turn — the message with what the
    /// speaker left implicit written in — when it wrote one that says
    /// something [`turn_text`] does not (ingest only; ≤ [`TURN_TEXT_CAP`]
    /// bytes).
    ///
    /// This is the sentence the hits below answer, on the turns where recall
    /// is hardest: *«l'ho comprato»* matches nothing on its own words.
    ///
    /// [`turn_text`]: Self::turn_text
    pub completed_message: Option<String>,
    /// The turn's classified intent (ingest only).
    pub intent: Option<String>,
    /// The depth the consumer asked for ([`crate::ingest::RecallDepth`]):
    /// `full`, or `light` for a turn that asked to skip the navigator's walk.
    /// `None` on a `wiki_navigate` trace, which has no such dial, and on a
    /// row written before the field.
    ///
    /// Without it a light turn is indistinguishable from one the intent
    /// skipped, and a surface explaining the missing walk explains it wrongly.
    pub recall_depth: Option<String>,
    /// Where the topic/subject seeds came from: `classifier` (ingest),
    /// `caller` / `query_extraction` / `rag_only` (the `wiki_navigate`
    /// cascade), `guest` (the ephemeral guest turn — no classifier runs).
    ///
    /// `rag_only` means the funnel started from the flat recall hits and
    /// nothing else — no navigator slot configured, or an extraction that
    /// returned neither topic nor subject.
    pub seed_mode: String,
    /// Topic seeds that fed the entry-point gather.
    pub topics: Vec<String>,
    /// Subject seeds (principal strings) that fed the gather.
    #[serde(alias = "owners")]
    pub subjects: Vec<String>,
    /// Flat vector-recall hits (promoted facts), scored, in recall order.
    pub flat_hits: Vec<TraceHit>,
    /// `true` when [`flat_hits`] are the second search's — the one run on
    /// [`completed_message`], which takes the place of the first.
    ///
    /// `false` with a [`completed_message`] present is its own answer: the
    /// second search failed and the first stands.
    ///
    /// [`flat_hits`]: Self::flat_hits
    /// [`completed_message`]: Self::completed_message
    pub flat_hits_from_completed: bool,
    /// Fresh-slot hits (un-promoted buffered captures; no page region).
    pub fresh_hits: Vec<TraceHit>,
    /// Due-soon slot hits (validity-imminent facts).
    pub due_soon: Vec<TraceHit>,
    /// The project-documentation slot, both halves, in the order they were
    /// pulled (ingest only).
    pub project_docs: Vec<TraceDocSection>,
    /// Identity cards handed to the consumer whole — the speaker's and the
    /// third parties' (ingest only).
    ///
    /// They cost no completion and arrive whatever the navigator decides, and
    /// their pages go into the walk's visited set, so the walk is never
    /// offered them. Without this the trace shows two whole sections of the
    /// block arriving from nowhere, and a page the walk could not have opened
    /// reads like one it declined.
    pub served_pages: Vec<TraceServedPage>,
    /// The deduplicated, weight-sorted entry-point fan.
    pub entry_points: Vec<TraceEntryPoint>,
    /// The funnel journal — one entry per navigator decision.
    pub hops: Vec<HopTrace>,
    /// Why the funnel ended ([`crate::recall_nav::NavStop`] token); `None` =
    /// navigation never ran (no navigator slot, or the intent skipped it).
    pub nav_stop: Option<String>,
    /// The navigation prose budget in force.
    pub char_budget: usize,
    /// Prose actually collected (same accounting unit as the budget).
    pub chars_collected: usize,
    /// `true` when the budget cut a page short.
    pub truncated: bool,
    /// The recalled-memory block exactly as injected (the assembled
    /// `context_snippet`, or the `wiki_navigate` result payload).
    pub injected_block: Option<String>,
    /// The behaviour-rules field injected alongside (ingest only).
    pub rules_block: Option<String>,
    /// The facts the reconciliation stage was shown (ingest only) — the union
    /// of the flat hits, every readable fact on every page the turn injected,
    /// and the still-buffered captures.
    ///
    /// Empty means the stage never ran: a turn that captured nothing, or one
    /// whose candidate union was empty and made no call. That is a different
    /// thing from a stage that ran and changed nothing, and the verdict beside
    /// it is what tells them apart.
    pub reconcile_candidates: Vec<TraceReconcileCandidate>,
    /// The reconciler's answer exactly as the model wrote it, before parsing
    /// (ingest only; `None` when no call was made or the model was
    /// unreachable).
    ///
    /// Raw on purpose. This is the only call in a turn that can retire a
    /// stored fact, and a parsed summary of it cannot say why an entry was
    /// refused — a hallucinated id, a missing slot, an owner the speaker is
    /// not — because the refusal happens after the parse and drops the entry.
    pub reconcile_verdict: Option<String>,
    /// Milliseconds of [`took_ms`] spent on the recall itself: the searches,
    /// the slots and the walk, summed as the turn ran.
    ///
    /// On a `wiki_navigate` trace it equals [`took_ms`] — the whole call is
    /// recall. On an `ingest` turn it does not, and the difference is the
    /// point: the classifier call, the reconciliation stage and the turn's
    /// own writes are inside the other number, and a single figure printed
    /// over a recall trace is read as the cost of the recall.
    ///
    /// [`took_ms`]: Self::took_ms
    pub recall_ms: u64,
    /// Wall-clock of the whole run, milliseconds — for an `ingest` trace the
    /// **whole turn**, writes included, not the recall ([`recall_ms`]).
    ///
    /// [`recall_ms`]: Self::recall_ms
    pub took_ms: u64,
}

/// One fact the reconciliation stage was shown, as journaled.
///
/// Id and a short text, and deliberately nothing else: what a reader of the
/// trace asks of this list is *was the fact the message contradicted even put
/// in front of the stage*, and that question is answered by the id being
/// there or not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceReconcileCandidate {
    /// Fact id, as the stage saw it.
    pub fact_id: String,
    /// Region body, capped at [`RECONCILE_TEXT_CAP`] bytes.
    pub text: String,
}

impl TraceReconcileCandidate {
    /// Journal one candidate, capping the body.
    #[must_use]
    pub fn from_hit(h: &RecallHit) -> Self {
        Self {
            fact_id: h.fact_id.as_str().to_owned(),
            text: cap_text(&h.text, RECONCILE_TEXT_CAP),
        }
    }
}

/// One page the identity slot served whole, as journaled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceServedPage {
    /// Wiki the card lives in.
    pub wiki_id: String,
    /// Page path relative to the wiki directory.
    pub page: String,
    /// Which slot served it: `speaker` (`WHO IS SPEAKING`) or `mentioned`
    /// (`PEOPLE THIS TURN IS ABOUT`).
    pub role: String,
}

/// One project-documentation section as journaled.
///
/// A section of a smart wiki's page, not a fact: it has no id, no validity
/// window and no seat in the flat block, which is why it is not a
/// [`TraceHit`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceDocSection {
    /// Containing wiki.
    pub wiki_id: String,
    /// Source file path, workdir-relative.
    pub source_path: String,
    /// Position of the section on its page — the other half of its identity.
    pub section_ord: i64,
    /// Heading chain, when the section sits under one.
    pub heading_path: Option<String>,
    /// Section body (≤ [`HIT_TEXT_CAP`] bytes).
    pub text: String,
    /// Cosine similarity against the query.
    pub score: f32,
    /// Which half pulled it: `named` (the message named the project, pulled
    /// before the classifier) or `signposted` (a signpost in the block, and
    /// the classifier judged the docs worth reading).
    pub half: String,
}

impl TraceDocSection {
    /// Journal one [`crate::recall::SectionHit`], capping the body.
    #[must_use]
    pub fn from_section(h: &crate::recall::SectionHit, half: &str) -> Self {
        Self {
            wiki_id: h.wiki_id.clone(),
            source_path: h.source_path.clone(),
            section_ord: h.section_ord,
            heading_path: h.heading_path.clone(),
            text: cap_text(&h.text, HIT_TEXT_CAP),
            score: h.score,
            half: half.to_owned(),
        }
    }
}

/// One recalled hit as journaled — the fields the trace viewer needs to
/// place it on its page (offsets) and rank it (score), body capped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceHit {
    /// Fact id.
    pub fact_id: String,
    /// Containing wiki.
    pub wiki_id: String,
    /// Source file path, workdir-relative.
    pub source_path: String,
    /// Byte offsets of the region on the published page (`None` on fresh
    /// hits and pre-offset rows).
    pub region_start: Option<i64>,
    /// One past the closing marker.
    pub region_end: Option<i64>,
    /// Region body (≤ [`HIT_TEXT_CAP`] bytes).
    pub text: String,
    /// Score the recall mechanism assigned (cosine; 1.0 on SQL pulls).
    pub score: f32,
    /// End of the validity window, when one exists (the due-soon signal).
    pub valid_to: Option<String>,
    /// The fact's type tag, when it carries one. It is what the per-kind seat
    /// is handed out on, so a reader shown that seat can see what earned it.
    pub fact_type: Option<String>,
    /// Which seat of the flat block this hit took
    /// ([`crate::recall::HitSeat::as_str`]): `similarity`,
    /// `macrotopic_quota`, `one_fact_per_kind`. `None` on a hit that never
    /// went through that allocation — a fresh capture, the due-soon slot — and
    /// on a row written before the field.
    pub seat: Option<String>,
    /// `true` when a link key scored this fact higher than the fact's own
    /// text did, so [`score`] is the key's number and similarity alone would
    /// not have reached it.
    ///
    /// [`score`]: Self::score
    pub link_key_win: bool,
    /// The score the classifier's per-fact vote left this hit with, when the
    /// vote moved it. `None` = the classifier said nothing about it.
    pub voted_score: Option<f32>,
    /// Why the flat slot did not put this hit in the block — `rules_page`,
    /// `on_an_injected_page`, `relevance_floor`, or
    /// `intent_skipped_the_slot` when the turn's intent never rendered the
    /// slot at all — and `None` when the hit reached the consumer.
    ///
    /// Without it the trace lists, under the heading of what recall found,
    /// facts the consumer never received.
    pub dropped: Option<String>,
}

impl TraceHit {
    /// Journal one [`RecallHit`], capping the body.
    ///
    /// [`voted_score`] and [`dropped`] are the flat slot's verdict, made
    /// after the walk and after the classifier has voted, so they are stamped
    /// on by the caller that renders that slot rather than read from the hit.
    ///
    /// [`voted_score`]: Self::voted_score
    /// [`dropped`]: Self::dropped
    #[must_use]
    pub fn from_hit(h: &RecallHit) -> Self {
        Self {
            fact_id: h.fact_id.as_str().to_owned(),
            wiki_id: h.wiki_id.clone(),
            source_path: h.source_path.clone(),
            region_start: h.region_start,
            region_end: h.region_end,
            text: cap_text(&h.text, HIT_TEXT_CAP),
            score: h.score,
            valid_to: h.valid_to.clone(),
            fact_type: h.fact_type.clone(),
            seat: h.seat.map(|s| s.as_str().to_owned()),
            link_key_win: h.link_key_win,
            voted_score: None,
            dropped: None,
        }
    }
}

/// One entry-point of the fan, as journaled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceEntryPoint {
    /// Target wiki.
    pub wiki_id: String,
    /// The page this seed opens. `Option` only because the field is
    /// `#[serde(default)]` and an old journal row may not carry one —
    /// **`None` is not a landing**: a live [`EntryPoint`] always names a
    /// page, and a seed that names only a wiki opens nothing — recall opens
    /// pages, and a wiki has no page of its own to fall back to.
    ///
    /// [`EntryPoint`]: crate::recall_nav::EntryPoint
    pub page: Option<String>,
    /// Seed family (`rag` | `topic` | `situational`).
    pub origin: String,
    /// Fan weight, `0.0..=1.0`. A `rag` door's weight is its hit's score; a
    /// `topic` door is always [`crate::recall_nav::WEIGHT_TOPIC_PAGE`] and a
    /// `situational` one always
    /// [`crate::recall_nav::WEIGHT_SITUATIONAL_PAGE`].
    pub weight: f32,
    /// The fact whose text opened this door, on a `rag` seed and nothing
    /// else ([`EntryPoint::matched_fact`]).
    ///
    /// It is the one thread between the hits and the doors: without it the
    /// fan is a list of pages with no stated relation to what was found.
    pub matched_fact: Option<String>,
}

impl TraceEntryPoint {
    /// Journal one [`EntryPoint`].
    #[must_use]
    pub fn from_entry(e: &EntryPoint) -> Self {
        Self {
            wiki_id: e.wiki_id.clone(),
            page: Some(e.page.to_string_lossy().into_owned()),
            origin: origin_label(e.origin).to_owned(),
            weight: e.weight,
            matched_fact: e.matched_fact.clone(),
        }
    }
}

/// Seed-family label (mirrors the candidate-line labels the funnel shows).
const fn origin_label(origin: crate::recall_nav::EntryOrigin) -> &'static str {
    use crate::recall_nav::EntryOrigin;
    match origin {
        EntryOrigin::Rag => "rag",
        EntryOrigin::Topic => "topic",
        EntryOrigin::Situational => "situational",
    }
}

/// Cap a turn/query text for the journal.
#[must_use]
pub fn cap_turn_text(s: &str) -> String {
    cap_text(s, TURN_TEXT_CAP)
}

/// Leading slice of `s`, at most `cap` bytes, cut on a char boundary.
fn cap_text(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_owned();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// One `recall_traces` row as read back for the dashboard.
#[derive(Debug, Clone)]
pub struct TraceRow {
    /// Monotonic row id — the fetch key of the viewer page.
    pub id: i64,
    /// RFC-3339 insert stamp.
    pub created_at: String,
    /// Which surface produced it.
    pub source: TraceSource,
    /// Bare user id the recall ran as.
    pub sender_id: String,
    /// The raw versioned JSON payload.
    pub payload: String,
}

impl TraceRow {
    /// Decode the payload. Tolerant by construction (`serde(default)`
    /// everywhere), so an older row still renders.
    ///
    /// # Errors
    ///
    /// Surfaces the JSON decode failure (a hand-edited row).
    pub fn parse(&self) -> Result<RecallTrace> {
        serde_json::from_str(&self.payload).context("recall_trace: decode payload")
    }
}

/// Record one recall trace, then prune rows past `retention_days`.
///
/// Best-effort telemetry by contract: callers log-and-ignore the error
/// rather than propagating it into the recall path.
///
/// `retention_days` comes from the caller's resolved policy
/// (`IngestPolicy::trace_retention_days`) so the window is one operator
/// setting rather than a constant recompiled per deployment. A value of `0`
/// or less keeps the journal write and skips the prune — the same off-switch
/// idiom the other resource knobs use, and never a silent purge.
///
/// # Errors
///
/// Surfaces the serialize / insert / prune failure so the caller can log it.
pub async fn record_trace(
    pool: &SqlitePool,
    source: TraceSource,
    sender_id: &str,
    trace: &RecallTrace,
    retention_days: i64,
) -> Result<()> {
    let payload = serde_json::to_string(trace).context("recall_trace: encode payload")?;
    sqlx::query(
        "INSERT INTO recall_traces (created_at, source, sender_id, payload) VALUES (?, ?, ?, ?)",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(source.as_str())
    .bind(sender_id)
    .bind(payload)
    .execute(pool)
    .await
    .context("recall_trace: insert")?;

    if retention_days > 0 {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
        sqlx::query("DELETE FROM recall_traces WHERE created_at < ?")
            .bind(&cutoff)
            .execute(pool)
            .await
            .context("recall_trace: prune")?;
    }
    Ok(())
}

/// The raw `recall_traces` row, decoded before the source token is parsed.
#[derive(sqlx::FromRow)]
struct RawRow {
    id: i64,
    created_at: String,
    source: String,
    sender_id: String,
    payload: String,
}

impl From<RawRow> for TraceRow {
    fn from(r: RawRow) -> Self {
        Self {
            id: r.id,
            created_at: r.created_at,
            source: TraceSource::from_db(&r.source),
            sender_id: r.sender_id,
            payload: r.payload,
        }
    }
}

/// The most recent traces, newest first, capped at `limit`.
///
/// # Errors
///
/// Surfaces the select SQL failure.
pub async fn recent_traces(pool: &SqlitePool, limit: i64) -> Result<Vec<TraceRow>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, created_at, source, sender_id, payload \
         FROM recall_traces ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("recall_trace: recent_traces")?;
    Ok(rows.into_iter().map(TraceRow::from).collect())
}

/// One sender's most recent traces, newest first, capped at `limit`.
///
/// The scoped sibling of [`recent_traces`], and the one the journal page
/// uses unless the admin reveal switch is on. The filter belongs in the
/// query: fetching a deployment-wide page and discarding other senders'
/// rows in the handler would show each user a shrinking slice of their own
/// history as the deployment gets busier — which is exactly what the
/// retention window exists to stop.
///
/// # Errors
///
/// Surfaces the select SQL failure.
pub async fn recent_traces_for_sender(
    pool: &SqlitePool,
    sender_id: &str,
    limit: i64,
) -> Result<Vec<TraceRow>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, created_at, source, sender_id, payload \
         FROM recall_traces WHERE sender_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(sender_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("recall_trace: recent_traces_for_sender")?;
    Ok(rows.into_iter().map(TraceRow::from).collect())
}

/// One trace by id, or `None` when pruned / never existed. Powers the
/// viewer page.
///
/// # Errors
///
/// Surfaces the select SQL failure.
pub async fn get_trace(pool: &SqlitePool, id: i64) -> Result<Option<TraceRow>> {
    let row = sqlx::query_as::<_, RawRow>(
        "SELECT id, created_at, source, sender_id, payload \
         FROM recall_traces WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("recall_trace: get_trace")?;
    Ok(row.map(TraceRow::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        (dir, pool)
    }

    fn sample(turn: &str) -> RecallTrace {
        RecallTrace {
            version: TRACE_PAYLOAD_VERSION,
            turn_text: cap_turn_text(turn),
            seed_mode: "classifier".to_owned(),
            topics: vec!["cucina".to_owned()],
            nav_stop: Some("done".to_owned()),
            injected_block: Some("- (famiglia) fatto".to_owned()),
            ..RecallTrace::default()
        }
    }

    #[tokio::test]
    async fn records_reads_and_roundtrips() {
        let (_dir, pool) = pool().await;
        record_trace(
            &pool,
            TraceSource::Ingest,
            "franz",
            &sample("cosa cucino?"),
            DEFAULT_TRACE_RETENTION_DAYS,
        )
        .await
        .expect("record ingest");
        record_trace(
            &pool,
            TraceSource::Navigate,
            "alice",
            &sample("query"),
            DEFAULT_TRACE_RETENTION_DAYS,
        )
        .await
        .expect("record navigate");

        let rows = recent_traces(&pool, 10).await.expect("recent");
        assert_eq!(rows.len(), 2);
        // Newest first: the navigate trace leads.
        assert_eq!(rows[0].source, TraceSource::Navigate);
        assert_eq!(rows[0].sender_id, "alice");
        assert_eq!(rows[1].source, TraceSource::Ingest);

        let parsed = rows[1].parse().expect("payload decodes");
        assert_eq!(parsed.version, TRACE_PAYLOAD_VERSION);
        assert_eq!(parsed.turn_text, "cosa cucino?");
        assert_eq!(parsed.topics, vec!["cucina".to_owned()]);
        assert_eq!(parsed.nav_stop.as_deref(), Some("done"));

        let one = get_trace(&pool, rows[0].id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(one.sender_id, "alice");
        assert!(get_trace(&pool, 999_999).await.expect("get").is_none());
    }

    /// Seed a row with a hand-chosen age, bypassing `record_trace`'s clock.
    async fn seed_aged(pool: &SqlitePool, sender_id: &str, age_days: i64, turn: &str) {
        let created = (chrono::Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        sqlx::query(
            "INSERT INTO recall_traces (created_at, source, sender_id, payload) \
             VALUES (?, 'ingest', ?, ?)",
        )
        .bind(&created)
        .bind(sender_id)
        .bind(serde_json::to_string(&sample(turn)).expect("encode"))
        .execute(pool)
        .await
        .expect("seed");
    }

    /// Age, not row count, is what retires a trace — the property the
    /// rewiring pass depends on, since ten rows is under an hour of traffic.
    #[tokio::test]
    async fn prunes_only_rows_past_the_retention_window() {
        let (_dir, pool) = pool().await;
        seed_aged(&pool, "franz", 45, "stale").await;
        seed_aged(&pool, "franz", 5, "recent").await;

        record_trace(&pool, TraceSource::Ingest, "franz", &sample("now"), 30)
            .await
            .expect("record");

        let turns: Vec<String> = recent_traces(&pool, 100)
            .await
            .expect("recent")
            .iter()
            .map(|r| r.parse().expect("payload").turn_text)
            .collect();
        assert_eq!(turns, vec!["now".to_owned(), "recent".to_owned()]);
    }

    /// `0` disables the prune. It must never read as "keep nothing" — an
    /// off-switch that purges the journal would be the worst reading of it.
    #[tokio::test]
    async fn a_zero_window_prunes_nothing() {
        let (_dir, pool) = pool().await;
        seed_aged(&pool, "franz", 4_000, "ancient").await;

        record_trace(&pool, TraceSource::Ingest, "franz", &sample("now"), 0)
            .await
            .expect("record");

        assert_eq!(recent_traces(&pool, 100).await.expect("recent").len(), 2);
    }

    /// The journal page scopes in SQL, so one sender's history does not
    /// thin out as other senders fill the window.
    #[tokio::test]
    async fn sender_scoped_listing_sees_only_its_own() {
        let (_dir, pool) = pool().await;
        for i in 0..5 {
            seed_aged(&pool, "alice", 1, &format!("alice {i}")).await;
        }
        seed_aged(&pool, "franz", 1, "franz only").await;

        let mine = recent_traces_for_sender(&pool, "franz", 3)
            .await
            .expect("scoped");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].sender_id, "franz");
        assert_eq!(mine[0].parse().expect("payload").turn_text, "franz only");
    }

    /// An older/foreign payload (missing fields, unknown extras) still
    /// decodes — the `serde(default)` tolerance the versioning relies on.
    #[test]
    fn payload_decode_is_tolerant() {
        let row = TraceRow {
            id: 1,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
            source: TraceSource::Ingest,
            sender_id: "franz".to_owned(),
            payload: r#"{"version":0,"turn_text":"old","unknown_field":true}"#.to_owned(),
        };
        let parsed = row.parse().expect("tolerant decode");
        assert_eq!(parsed.turn_text, "old");
        assert!(parsed.flat_hits.is_empty());
        assert!(parsed.injected_block.is_none());
    }

    /// A **version 1** row, whole, as that shape wrote it. The retention
    /// window is months long, so rows of the previous shape are on screen
    /// long after a field lands: every one of them decodes, and every field
    /// version 2 added answers "absent" rather than answering wrongly.
    #[test]
    fn a_version_one_row_still_decodes_and_says_it_knows_none_of_this() {
        let payload = r#"{
            "version": 1,
            "consumer": null,
            "turn_text": "cosa cucino stasera?",
            "intent": "recall",
            "seed_mode": "classifier",
            "topics": ["cucina"],
            "subjects": ["user:alice"],
            "flat_hits": [{
                "fact_id": "0197fa00-0000-7000-8000-000000000001",
                "wiki_id": "famiglia",
                "source_path": "wikis/famiglia/cucina.md",
                "region_start": 120,
                "region_end": 180,
                "text": "carol è celiaca",
                "score": 0.83,
                "valid_to": null
            }],
            "fresh_hits": [],
            "due_soon": [],
            "entry_points": [{
                "wiki_id": "famiglia",
                "page": "cucina.md",
                "origin": "rag",
                "weight": 0.83
            }],
            "hops": [{
                "candidates": [],
                "requested": [{"wiki_id": "famiglia", "page": null, "opened": false}],
                "done": true,
                "note": "parto dalla cucina",
                "opened": []
            }],
            "nav_stop": "done",
            "char_budget": 8000,
            "chars_collected": 420,
            "truncated": false,
            "injected_block": "RELEVANT MEMORY",
            "rules_block": null,
            "took_ms": 2150
        }"#;
        let row = TraceRow {
            id: 1,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
            source: TraceSource::Ingest,
            sender_id: "alice".to_owned(),
            payload: payload.to_owned(),
        };
        let t = row.parse().expect("a version 1 row decodes");

        // What version 1 carried is read exactly as it was written.
        assert_eq!(t.version, 1);
        assert_eq!(t.turn_text, "cosa cucino stasera?");
        assert_eq!(t.flat_hits.len(), 1);
        assert_eq!(t.nav_stop.as_deref(), Some("done"));
        assert_eq!(t.took_ms, 2150);

        // What version 2 added is absent, and absent is a shape a surface can
        // render: no producer, no depth, no completed message, no slot, no
        // seat, no refusal, no recall-only figure.
        assert!(t.producer.is_empty());
        assert_eq!(t.recall_depth, None);
        assert_eq!(t.completed_message, None);
        assert!(!t.flat_hits_from_completed);
        assert!(t.project_docs.is_empty());
        assert!(t.served_pages.is_empty());
        assert_eq!(t.recall_ms, 0);
        assert_eq!(t.flat_hits[0].fact_type, None);
        assert_eq!(t.flat_hits[0].seat, None);
        assert!(!t.flat_hits[0].link_key_win);
        assert_eq!(t.flat_hits[0].voted_score, None);
        assert_eq!(t.flat_hits[0].dropped, None);
        assert_eq!(t.entry_points[0].matched_fact, None);
        assert_eq!(t.hops[0].requested[0].reason, None);
    }

    /// The version number is only worth reading if its own doc says what each
    /// version added. This literal is the tripwire: a shape change that bumps
    /// the constant lands here, one line under the doc it has to extend.
    #[test]
    fn the_payload_version_names_every_shape_it_has_had() {
        assert_eq!(
            TRACE_PAYLOAD_VERSION, 3,
            "bumping this means adding the new version's line to the constant's doc"
        );
    }

    /// Multi-byte text is capped on a char boundary, never mid-codepoint.
    #[test]
    fn cap_text_respects_char_boundaries() {
        let s = "è".repeat(400); // 2 bytes each
        let capped = cap_text(&s, 501);
        assert!(capped.len() <= 501);
        assert!(capped.chars().all(|c| c == 'è'));
    }
}
