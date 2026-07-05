// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded journal of recall runs — the story behind the admin Traces page.
//!
//! A "recall trace" records the **whole route** one recall took: the flat /
//! fresh / due-soon hits, the entry-point fan, every navigator hop (the
//! candidates offered, the decision with its one-line note, the pages that
//! actually opened) and the block that was finally injected into the
//! consumer. Two producers write here (see the
//! [recall pipeline](../../../wiki/design-notes/recall-pipeline.md)):
//!
//! - the **ingest per-turn injection** ([`crate::ingest::wiki_ingest_message`])
//!   — what a bridge-connected consumer received for a user's turn;
//! - the **`wiki_navigate` tool** — what an explicitly-searching consumer got
//!   back from deep recall.
//!
//! The journal is deliberately tiny: [`record_trace`] prunes to the newest
//! [`TRACE_KEEP`] rows after every insert (a resource cap — the surface is
//! "the last few recalls", not an audit trail; `tool_executions` remains the
//! audit surface). Recording is best-effort telemetry: a journal write must
//! never fail or mask the recall itself, so callers log-and-ignore the error.
//!
//! The payload is versioned JSON ([`RecallTrace`], `version` = 1) stored in
//! the `recall_traces` table (migration `0057`); every field is
//! `serde(default)` so a reader tolerates rows written by an older shape.
//! Traces cross wiki and ACL lines by construction (they journal what a
//! specific sender was served), so the dashboard surface that renders them is
//! **admin-only**.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::recall::RecallHit;
use crate::recall_nav::{EntryPoint, HopTrace};

/// How many traces the journal keeps. Older rows are pruned after each insert.
pub const TRACE_KEEP: i64 = 10;

/// Payload schema version written by this build.
pub const TRACE_PAYLOAD_VERSION: u32 = 1;

/// Byte cap on the journaled turn / query text.
const TURN_TEXT_CAP: usize = 1_200;
/// Byte cap on each journaled hit body.
const HIT_TEXT_CAP: usize = 500;

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
    /// Consumer that carried the call, when the transport knew one
    /// (`wiki_navigate` fills it from the token; the ingest path has no
    /// consumer claim in scope and leaves it empty).
    pub consumer: Option<String>,
    /// The turn / query text (≤ [`TURN_TEXT_CAP`] bytes).
    pub turn_text: String,
    /// The turn's classified intent (ingest only).
    pub intent: Option<String>,
    /// Where the topic/owner seeds came from: `classifier` (ingest),
    /// `caller` / `query_extraction` / `principal_rag_only` (the
    /// `wiki_navigate` cascade), `guest` (the ephemeral guest turn — no
    /// classifier runs).
    pub seed_mode: String,
    /// Topic seeds that fed the entry-point gather.
    pub topics: Vec<String>,
    /// Owner seeds (principal strings) that fed the gather.
    pub owners: Vec<String>,
    /// Flat vector-recall hits (promoted facts), scored.
    pub flat_hits: Vec<TraceHit>,
    /// Fresh-slot hits (un-promoted buffered captures; no page region).
    pub fresh_hits: Vec<TraceHit>,
    /// Due-soon slot hits (validity-imminent facts).
    pub due_soon: Vec<TraceHit>,
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
    /// Wall-clock of the whole run, milliseconds.
    pub took_ms: u64,
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
}

impl TraceHit {
    /// Journal one [`RecallHit`], capping the body.
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
        }
    }
}

/// One entry-point of the fan, as journaled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceEntryPoint {
    /// Target wiki.
    pub wiki_id: String,
    /// `None` = the wiki root (its overview page).
    pub page: Option<String>,
    /// Seed family (`principal` | `rag` | `topic` | `situational`).
    pub origin: String,
    /// Fan weight, `0.0..=1.0`.
    pub weight: f32,
}

impl TraceEntryPoint {
    /// Journal one [`EntryPoint`].
    #[must_use]
    pub fn from_entry(e: &EntryPoint) -> Self {
        Self {
            wiki_id: e.wiki_id.clone(),
            page: e.page.as_ref().map(|p| p.to_string_lossy().into_owned()),
            origin: origin_label(e.origin).to_owned(),
            weight: e.weight,
        }
    }
}

/// Seed-family label (mirrors the candidate-line labels the funnel shows).
const fn origin_label(origin: crate::recall_nav::EntryOrigin) -> &'static str {
    use crate::recall_nav::EntryOrigin;
    match origin {
        EntryOrigin::Principal => "principal",
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

/// Record one recall trace, then prune the journal to [`TRACE_KEEP`].
///
/// Best-effort telemetry by contract: callers log-and-ignore the error
/// rather than propagating it into the recall path.
///
/// # Errors
///
/// Surfaces the serialize / insert / prune failure so the caller can log it.
pub async fn record_trace(
    pool: &SqlitePool,
    source: TraceSource,
    sender_id: &str,
    trace: &RecallTrace,
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

    // Keep only the newest TRACE_KEEP rows. `id` is a monotonic
    // autoincrement, so ordering by it descending is newest-first.
    sqlx::query(
        "DELETE FROM recall_traces \
         WHERE id NOT IN (SELECT id FROM recall_traces ORDER BY id DESC LIMIT ?)",
    )
    .bind(TRACE_KEEP)
    .execute(pool)
    .await
    .context("recall_trace: prune")?;
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
        record_trace(&pool, TraceSource::Ingest, "franz", &sample("cosa cucino?"))
            .await
            .expect("record ingest");
        record_trace(&pool, TraceSource::Navigate, "alice", &sample("query"))
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

    #[tokio::test]
    async fn prunes_to_trace_keep() {
        let (_dir, pool) = pool().await;
        for i in 0..(TRACE_KEEP + 4) {
            record_trace(
                &pool,
                TraceSource::Ingest,
                "franz",
                &sample(&format!("turn {i}")),
            )
            .await
            .expect("record");
        }
        let rows = recent_traces(&pool, TRACE_KEEP + 100)
            .await
            .expect("recent");
        assert_eq!(rows.len(), usize::try_from(TRACE_KEEP).unwrap());
        let newest = rows[0].parse().expect("payload");
        assert_eq!(newest.turn_text, format!("turn {}", TRACE_KEEP + 3));
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

    /// Multi-byte text is capped on a char boundary, never mid-codepoint.
    #[test]
    fn cap_text_respects_char_boundaries() {
        let s = "è".repeat(400); // 2 bytes each
        let capped = cap_text(&s, 501);
        assert!(capped.len() <= 501);
        assert!(capped.chars().all(|c| c == 'è'));
    }
}
