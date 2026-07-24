// SPDX-License-Identifier: AGPL-3.0-or-later
//! `wiki_events` queue — append-only event log.
//!
//! Emitted by REM; drained by consumers via `events_poll` /
//! `events_ack`.
//!
//! ## Why this module exists
//!
//! Event inserts come from [`rem`](crate::rem) sub-jobs
//! (`promotion_applied`, `dedup_proposed`) and any future tool that
//! surfaces an asynchronous notification. They
//! share the same shape and the same idempotency concern (don't fire
//! the same notification twice in two REM cycles for the same fact), so
//! the boring SQL is centralised here.
//!
//! ## Idempotency
//!
//! Lifecycle rules typically wake on a condition that stays true across
//! several REM cycles (e.g. `due_at < now AND status = pending`). The
//! interpreter must not emit the same event every night until the
//! operator marks the fact as `done`. [`find_recent_event_for`] is the
//! cheap pre-check used by lifecycle: if a matching `(kind, fact_id)`
//! row already exists in the past N days, skip the insert.
//!
//! The dedup window is 30 days (the same as the
//! consumer-ack retention default in engine DB and
//! migrations). A consumer that acks an
//! event and wants to be re-notified later flips the underlying state
//! (e.g. resets `status: pending` → `done` → `pending`), at which point
//! the natural cadence resumes.
//!
//! ## Consumer-side queue
//!
//! [`poll_events`] / [`ack_events`] are the read side: consumers
//! registered via [`crate::consumers::register`] drain the queue
//! filtered by their `consumer_id`. Per-row delivery state lives in the
//! `acks` JSON map (`{ consumer_id: ack_ts }`) — a row is "pending for
//! consumer X" iff `acks->>X` is absent. Filtering happens server-side
//! via `SQLite`'s JSON1 `json_extract`.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Wire-stable identifiers for event kinds.
///
/// Emitted by REM and the ingest paths. New variants are additive — the
/// `kind` column is `TEXT`, so a future consumer that does not know a
/// kind just receives the JSON payload and decides what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// REM jaccard-scan flagged a candidate dedup pair and the LLM
    /// confirmed; the new fact superseded the older one. Audit-only —
    /// the supersede itself is already in `fact_index`.
    DedupProposed,
    /// REM applied a structural change **directly** (paragraph→page
    /// split or page→sub-wiki emergence) and recorded a born-applied
    /// undo receipt. This is the **notice**: the system never blocks on
    /// approval for a structural edit — it acts and tells you. The
    /// payload carries the receipt `proposal_id` (the undo anchor), the
    /// `variant`, source → target, the `revert_deadline`, a
    /// `dashboard_path` for the undo surface, and — crucially —
    /// `recipient_id`, the **affected user** (owner/sender of the
    /// changed wiki), so a multi-user consumer agent knows whom to
    /// forward the notice to. The dashboard is the *undo* surface, not
    /// the notification surface: the notice reaches the causing agent
    /// here, over `events_poll`.
    StructureApplied,
    /// REM's archive detector inserted a pending `archive_proposals`
    /// row (a page whose every active fact went stale). Unlike the
    /// structural rungs above, archival still rides the proposal
    /// lifecycle — the payload carries the `proposal_id`, `path`, and
    /// `reason` so a consumer can surface it.
    ArchiveProposed,
    /// Auto-apply sweep moved a `pending` proposal to
    /// `applied_pending_confirm`. Payload includes `proposal_id`,
    /// `summary`, `confirm_deadline`, and `dashboard_path`, so the
    /// consumer can prompt the user with "we did X — confirm or revert
    /// within N days".
    ///
    /// The `confirm_window` policy (silence =
    /// consent) means there is no symmetric `AutoReverted` event: when
    /// the `confirm_window` elapses without user action, the finalize
    /// sweep flips the row silently to `applied` without emitting
    /// anything — the user has already been notified at auto-apply
    /// time and silence is now a valid form of consent.
    AutoApplied,
    /// A document-ingest job finished
    /// (document ingest).
    /// The payload carries `job_id`, the resolved `disposition` and
    /// `title`, the anchor `document_page` (consult/dossier — absent on
    /// dissolve), `facts_buffered`, and the `source_ref`, so the consumer
    /// that enqueued the job can tell the user what the memory now holds.
    DocumentIngested,
    /// The narrative compiler failed (or degraded) the **same page** in
    /// consecutive compile passes — the per-page failure ledger
    /// ([`crate::compile_failures`]) hit a notice threshold. Emitted once
    /// per threshold per streak (at exactly 2, again at exactly 5), so a
    /// persistently-failing page reaches the operator instead of living
    /// only in the run's report dump. The payload carries the plan `slug`,
    /// the workdir-relative `source_path`, the `consecutive` count, the
    /// `last_error`, and a `dashboard_path` to the Dream console (the run
    /// history holds the full log).
    CompileFailureStreak,
    /// The recall-repair sub-job found the **same fact** missing from
    /// recall repeatedly and no local (re-file) repair committed — the
    /// operator review-queue entry of self-correcting REM. Rule / prompt
    /// / recall-knob levers are the highest-blast-radius fixes in the
    /// system, so they are **never auto-applied**: this notice carries
    /// the evidence (the fact, its home, the miss count, the sample
    /// queries, and the gate outcome when a candidate repair was tried)
    /// and the operator decides.
    RecallTuningProposed,
    /// Ingest filed a fact **owned by an enrolled human who was not the
    /// human of that turn** (or, on the document path, not the
    /// uploader) — the delivery half of the subject-owner axiom: the
    /// beneficiary should be TOLD a fact was minted for them out of
    /// someone else's conversation, not discover it on their next
    /// recall. Batched per (beneficiary, ingest call): one event carries
    /// every fact the turn minted for that user, so a five-fact turn is
    /// one notice, not five pings. The payload carries `recipient_id`
    /// (`user:`-prefixed), `from_user_id` (bare id of the human whose
    /// turn or upload minted it), `origin` (`user_turn` |
    /// `assistant_turn` | `document`), a `facts` array (`fact_id`,
    /// `wiki_id`, `body` — the content itself, so the bridge's agent can
    /// deliver it without a recall round-trip), and a `dashboard_path`.
    /// Only real humans are addressed: group-owned facts are communal
    /// and agent principals (`is_agent`) have no inbox.
    FactMintedForYou,
}

impl EventKind {
    /// Lowercase wire-stable string used as the `kind` column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DedupProposed => "dedup_proposed",
            Self::StructureApplied => "structure_applied",
            Self::ArchiveProposed => "archive_proposed",
            Self::AutoApplied => "auto_applied",
            Self::DocumentIngested => "document_ingested",
            Self::CompileFailureStreak => "compile_failure_streak",
            Self::RecallTuningProposed => "recall_tuning_proposed",
            Self::FactMintedForYou => "fact_minted_for_you",
        }
    }
}

/// Errors raised by the events layer.
#[derive(Debug, Error)]
pub enum EventsError {
    /// Underlying SQL failure (insert / select).
    #[error("events db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON serialisation of the payload failed.
    #[error("events payload: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, EventsError>;

/// A row in `wiki_events`. Trimmed shape — the `acks` JSON map is
/// the consumer-ack tracker and not relevant to the
/// emitter.
#[derive(Debug, Clone)]
pub struct WikiEvent {
    /// Auto-increment primary key.
    pub id: i64,
    /// Event kind (parsed back into an enum at the boundary).
    pub kind: String,
    /// Wiki this event is about, if any (omitted for global events).
    pub wiki_id: Option<String>,
    /// Fact this event was triggered by, if any.
    pub fact_id: Option<String>,
    /// JSON payload — kind-specific shape.
    pub payload: Option<String>,
    /// ISO 8601 wall-clock when the row was inserted.
    pub created_at: String,
}

/// Insert a new event row.
///
/// `payload` is serialised to JSON; passing `&serde_json::Value::Null`
/// stores SQL `NULL` so polling consumers can omit it from their
/// payload shape.
///
/// # Errors
///
/// - [`EventsError::Json`] when payload serialisation fails.
/// - [`EventsError::Db`] for any SQL failure.
pub async fn insert_event(
    pool: &SqlitePool,
    kind: EventKind,
    wiki_id: Option<&str>,
    fact_id: Option<&str>,
    payload: &serde_json::Value,
) -> Result<i64> {
    let payload_str = if payload.is_null() {
        None
    } else {
        Some(serde_json::to_string(payload)?)
    };
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_events (kind, wiki_id, fact_id, payload, created_at)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(kind.as_str())
    .bind(wiki_id)
    .bind(fact_id)
    .bind(payload_str)
    .bind(&now)
    .fetch_one(pool)
    .await?;
    tracing::info!(
        kind = kind.as_str(),
        wiki_id,
        fact_id,
        event_id = row.0,
        "events: inserted"
    );
    Ok(row.0)
}

/// Idempotency probe used by the lifecycle interpreter.
///
/// Returns `true` when a row with the same `(kind, fact_id)` was
/// emitted within `window` (typically 30 days). The caller short-
/// circuits to skip the insert.
///
/// `fact_id` is required — global events are not deduplicated by this
/// helper (they have no natural key beyond `kind`, and the lifecycle
/// interpreter today only fires per-region events).
///
/// # Errors
///
/// - [`EventsError::Db`] for any SQL failure.
pub async fn find_recent_event_for(
    pool: &SqlitePool,
    kind: EventKind,
    fact_id: &str,
    window: chrono::Duration,
) -> Result<bool> {
    let cutoff = (chrono::Utc::now() - window).to_rfc3339();
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM wiki_events
          WHERE kind = ? AND fact_id = ? AND created_at >= ?
          ORDER BY id DESC
          LIMIT 1",
    )
    .bind(kind.as_str())
    .bind(fact_id)
    .bind(&cutoff)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Output row for [`poll_events`].
///
/// Shape that downstream MCP tool callers want to surface to consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolledEvent {
    /// Auto-increment id, opaque but stable; used in [`ack_events`].
    pub event_id: i64,
    /// Wire-stable kind string. Always present, may be a custom kind
    /// outside [`EventKind`] (the column is `TEXT`).
    pub kind: String,
    /// Wiki this event is about, if any.
    pub wiki_id: Option<String>,
    /// Fact this event was triggered by, if any.
    pub fact_id: Option<String>,
    /// Decoded JSON payload, or [`serde_json::Value::Null`] if absent.
    pub payload: serde_json::Value,
    /// ISO 8601 wall-clock when the event was inserted (server-side).
    pub emitted_at: String,
}

/// Outcome of [`poll_events`]: returned events + a `has_more` hint so a
/// consumer can decide whether to call again immediately.
#[derive(Debug, Clone)]
pub struct PollOutcome {
    /// Events delivered to this consumer in this call. Ordered by
    /// `emitted_at ASC` (oldest first) so a single-pass consumer
    /// preserves event order.
    pub events: Vec<PolledEvent>,
    /// `true` when more pending events exist past `top_k` — the
    /// consumer should poll again right after acking these.
    pub has_more: bool,
}

/// Outcome of [`ack_events`]: how many ids were stamped + which ones
/// could not be found (the JSON map is unchanged for those).
#[derive(Debug, Clone, Default)]
pub struct AckOutcome {
    /// Number of `event_id`s whose `acks[consumer_id]` was set (or
    /// refreshed). A re-ack counts: the operation is idempotent at the
    /// JSON-map level (we overwrite the timestamp), at the row level
    /// the consumer never sees the event again.
    pub acked: usize,
    /// `event_id`s the client passed that did not correspond to any
    /// row (already GC'd, never existed). The caller surfaces these as
    /// the `unknown` field of the tool response.
    pub unknown: Vec<i64>,
}

/// Default `top_k` for [`poll_events`] — matches the
/// [tool reference](../../../docs/protocol/tool-reference.md) for `events_poll`.
pub const DEFAULT_POLL_TOP_K: i64 = 20;

/// Maximum `top_k` accepted by [`poll_events`] — same spec.
pub const MAX_POLL_TOP_K: i64 = 50;

/// Drain pending events for `consumer_id`.
///
/// Selection semantics (intersected, all optional except consumer):
/// - The consumer's `acks` slot is empty (`json_extract(acks, '$.<id>') IS NULL`).
/// - `created_at > since` when `since` is provided.
/// - `kind IN (...)` when `kinds` is non-empty.
/// - Ordered `created_at ASC`, `id ASC` for a stable tiebreaker.
/// - Returns at most `top_k.clamp(1, MAX_POLL_TOP_K)` rows.
///
/// `has_more` is `true` iff the underlying query returned `top_k + 1`
/// matches — the extra row is discarded.
///
/// # Errors
///
/// - [`EventsError::Db`] for any SQL failure.
/// - [`EventsError::Json`] if a stored payload is no longer valid JSON
///   (operationally a schema-drift problem, surfaced loudly).
pub async fn poll_events(
    pool: &SqlitePool,
    consumer_id: &str,
    since: Option<&str>,
    kinds: &[String],
    top_k: i64,
) -> Result<PollOutcome> {
    let limit = top_k.clamp(1, MAX_POLL_TOP_K);
    let probe_limit = limit + 1;

    // Build dynamic IN-list for kinds. We bind values so SQL injection
    // is impossible regardless of where the strings came from.
    let kinds_placeholder = if kinds.is_empty() {
        String::new()
    } else {
        let qs = std::iter::repeat_n("?", kinds.len())
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND kind IN ({qs})")
    };
    let since_clause = if since.is_some() {
        " AND created_at > ?"
    } else {
        ""
    };

    let sql = format!(
        "SELECT id, kind, wiki_id, fact_id, payload, created_at
           FROM wiki_events
          WHERE json_extract(acks, '$.' || ?) IS NULL{since_clause}{kinds_placeholder}
          ORDER BY created_at ASC, id ASC
          LIMIT ?"
    );

    let mut query = sqlx::query_as::<_, EventTuple>(&sql).bind(consumer_id);
    if let Some(s) = since {
        query = query.bind(s);
    }
    for k in kinds {
        query = query.bind(k);
    }
    let rows: Vec<EventTuple> = query.bind(probe_limit).fetch_all(pool).await?;

    let has_more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let kept = rows.into_iter().take(usize::try_from(limit).unwrap_or(0));
    let mut events = Vec::new();
    for (id, kind, wiki_id, fact_id, payload, created_at) in kept {
        let payload_val = match payload {
            Some(s) => serde_json::from_str(&s)?,
            None => serde_json::Value::Null,
        };
        events.push(PolledEvent {
            event_id: id,
            kind,
            wiki_id,
            fact_id,
            payload: payload_val,
            emitted_at: created_at,
        });
    }

    touch_consumer_last_seen(pool, consumer_id).await?;

    tracing::info!(
        consumer_id,
        delivered = events.len(),
        has_more,
        "events: poll"
    );
    Ok(PollOutcome { events, has_more })
}

/// Stamp `consumer_id` in the `acks` JSON map of every listed event.
///
/// - Re-acking is a no-op at the row level (we overwrite the timestamp)
///   and the consumer never sees the event again.
/// - Unknown `event_id` values are returned in [`AckOutcome::unknown`];
///   the caller surfaces them as the `unknown` field of the tool
///   response per the [tool reference](../../../docs/protocol/tool-reference.md) for `events_ack`.
///
/// ## Concurrency — why this transaction writes first
///
/// Each id is stamped with a single `UPDATE` that patches the `acks` JSON
/// **in place** via `json_set`; we never `SELECT` the blob into Rust and
/// write it back. That ordering is load-bearing. A `BEGIN DEFERRED`
/// transaction that reads before it writes acquires only a *read snapshot*
/// on its first `SELECT` and must *upgrade* to the write lock at the
/// `UPDATE`. If any other connection commits in the gap — e.g. a concurrent
/// [`poll_events`] stamping `last_seen_at`, or an ingest inserting an event
/// — `SQLite` refuses the upgrade with `SQLITE_BUSY_SNAPSHOT`, surfaced as
/// `(code: 5) database is locked`. Crucially the `busy_timeout` handler
/// (see [`crate::db::open_or_init`]) **cannot** retry a snapshot conflict,
/// so it fails *instantly* rather than waiting its turn. With a
/// consumer polling every 30s the read→write window was hit on almost every
/// ack. Writing first takes the write lock at the first `UPDATE` with no
/// snapshot to invalidate, so `busy_timeout` serialises contending writers
/// the normal way.
///
/// # Errors
///
/// - [`EventsError::Db`] for any SQL failure.
pub async fn ack_events(
    pool: &SqlitePool,
    consumer_id: &str,
    event_ids: &[i64],
) -> Result<AckOutcome> {
    if event_ids.is_empty() {
        return Ok(AckOutcome::default());
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut outcome = AckOutcome::default();

    let mut tx = pool.begin().await?;
    for &id in event_ids {
        // Patch `acks[consumer_id] = now` in place. `COALESCE(NULLIF(...))`
        // guards the (defensive) empty-string / NULL blob so `json_set`
        // always receives valid JSON. A missing row updates zero rows and
        // is reported as `unknown`; an existing row always reports one
        // changed row, so a re-ack stays idempotent (acked = 1).
        let affected = sqlx::query(
            "UPDATE wiki_events
                SET acks = json_set(COALESCE(NULLIF(acks, ''), '{}'), '$.' || ?, ?)
              WHERE id = ?",
        )
        .bind(consumer_id)
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 0 {
            outcome.unknown.push(id);
        } else {
            outcome.acked += 1;
        }
    }
    tx.commit().await?;

    touch_consumer_last_seen(pool, consumer_id).await?;

    tracing::info!(
        consumer_id,
        acked = outcome.acked,
        unknown = outcome.unknown.len(),
        "events: ack"
    );
    Ok(outcome)
}

/// Stamp `consumers.last_seen_at = now()` whenever a registered
/// consumer interacts with the queue. Best-effort: if the consumer was
/// never registered (or the table is somehow missing) we keep silent
/// — the auth layer is the gate for `consumer_not_registered`, not
/// this helper.
async fn touch_consumer_last_seen(pool: &SqlitePool, consumer_id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let _ = sqlx::query("UPDATE consumers SET last_seen_at = ? WHERE consumer_id = ?")
        .bind(&now)
        .bind(consumer_id)
        .execute(pool)
        .await?;
    Ok(())
}

type EventTuple = (
    i64,            // id
    String,         // kind
    Option<String>, // wiki_id
    Option<String>, // fact_id
    Option<String>, // payload (JSON or NULL)
    String,         // created_at
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn fresh_pool() -> SqlitePool {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        db::open_or_init(dir.path()).await.expect("db open")
    }

    #[test]
    fn event_kind_wire_strings_are_stable() {
        assert_eq!(EventKind::DedupProposed.as_str(), "dedup_proposed");
        assert_eq!(EventKind::StructureApplied.as_str(), "structure_applied");
        assert_eq!(EventKind::AutoApplied.as_str(), "auto_applied");
        assert_eq!(
            EventKind::CompileFailureStreak.as_str(),
            "compile_failure_streak"
        );
        assert_eq!(EventKind::FactMintedForYou.as_str(), "fact_minted_for_you");
    }

    #[tokio::test]
    async fn insert_event_persists_kind_and_payload() {
        let pool = fresh_pool().await;
        let payload = serde_json::json!({"due_at": "2026-05-19T09:00:00Z"});
        let id = insert_event(
            &pool,
            EventKind::DedupProposed,
            Some("alice"),
            Some("018f1234-5678-7abc-9def-0123456789ab"),
            &payload,
        )
        .await
        .expect("insert");

        let (kind, wiki, fact, payload_s, _ack): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT kind, wiki_id, fact_id, payload, acks FROM wiki_events WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch");
        assert_eq!(kind, "dedup_proposed");
        assert_eq!(wiki.as_deref(), Some("alice"));
        assert_eq!(
            fact.as_deref(),
            Some("018f1234-5678-7abc-9def-0123456789ab")
        );
        assert!(payload_s.unwrap().contains("2026-05-19T09:00:00Z"));
    }

    #[tokio::test]
    async fn insert_event_stores_null_for_null_payload() {
        let pool = fresh_pool().await;
        let id = insert_event(
            &pool,
            EventKind::StructureApplied,
            None,
            Some("018f1234-5678-7abc-9def-0123456789ab"),
            &serde_json::Value::Null,
        )
        .await
        .expect("insert");
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM wiki_events WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("fetch");
        assert!(payload.is_none());
    }

    #[tokio::test]
    async fn find_recent_event_detects_existing_pair_within_window() {
        let pool = fresh_pool().await;
        let fact_id = "018f1234-5678-7abc-9def-0123456789ac";
        insert_event(
            &pool,
            EventKind::DedupProposed,
            Some("alice"),
            Some(fact_id),
            &serde_json::Value::Null,
        )
        .await
        .expect("insert");
        let seen = find_recent_event_for(
            &pool,
            EventKind::DedupProposed,
            fact_id,
            chrono::Duration::days(30),
        )
        .await
        .expect("probe");
        assert!(seen, "row inserted moments ago must be visible");
    }

    #[tokio::test]
    async fn find_recent_event_misses_outside_window() {
        let pool = fresh_pool().await;
        let fact_id = "018f1234-5678-7abc-9def-0123456789ad";
        insert_event(
            &pool,
            EventKind::AutoApplied,
            Some("alice"),
            Some(fact_id),
            &serde_json::Value::Null,
        )
        .await
        .expect("insert");
        // Backdate it 31 days.
        let old = (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339();
        sqlx::query("UPDATE wiki_events SET created_at = ? WHERE fact_id = ?")
            .bind(&old)
            .bind(fact_id)
            .execute(&pool)
            .await
            .expect("backdate");
        let seen = find_recent_event_for(
            &pool,
            EventKind::AutoApplied,
            fact_id,
            chrono::Duration::days(30),
        )
        .await
        .expect("probe");
        assert!(!seen, "31-day-old row must not be visible in a 30d window");
    }

    /// Inserts a `consumers` row so poll/ack can stamp `last_seen_at`
    /// without surfacing an inconsistent state. Returns the id used.
    async fn register_dummy_consumer(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO consumers (consumer_id, consumer_secret, registered_at) \
             VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind("dummy-secret")
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(pool)
        .await
        .expect("dummy consumer insert");
    }

    #[tokio::test]
    async fn poll_events_returns_pending_in_chronological_order() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        insert_event(
            &pool,
            EventKind::DedupProposed,
            Some("alice"),
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();
        insert_event(
            &pool,
            EventKind::StructureApplied,
            Some("alice"),
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();

        let out = poll_events(&pool, "samvise", None, &[], DEFAULT_POLL_TOP_K)
            .await
            .expect("poll");
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].kind, "dedup_proposed");
        assert_eq!(out.events[1].kind, "structure_applied");
        assert!(!out.has_more);
    }

    #[tokio::test]
    async fn poll_events_filters_by_since_and_kinds() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        insert_event(
            &pool,
            EventKind::DedupProposed,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();
        // Backdate the first event by 1 day so `since=now-1h` excludes it.
        let old = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        sqlx::query("UPDATE wiki_events SET created_at = ?")
            .bind(&old)
            .execute(&pool)
            .await
            .unwrap();
        insert_event(
            &pool,
            EventKind::StructureApplied,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();

        let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let out = poll_events(&pool, "samvise", Some(&since), &[], DEFAULT_POLL_TOP_K)
            .await
            .unwrap();
        assert_eq!(
            out.events.len(),
            1,
            "only the fresh event survives the since filter"
        );
        assert_eq!(out.events[0].kind, "structure_applied");

        let out_filtered = poll_events(
            &pool,
            "samvise",
            None,
            &["dedup_proposed".to_owned()],
            DEFAULT_POLL_TOP_K,
        )
        .await
        .unwrap();
        assert_eq!(out_filtered.events.len(), 1);
        assert_eq!(out_filtered.events[0].kind, "dedup_proposed");
    }

    #[tokio::test]
    async fn poll_events_skips_already_acked_per_consumer() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        register_dummy_consumer(&pool, "telegram-bot").await;
        let id = insert_event(
            &pool,
            EventKind::DedupProposed,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();

        let _ = ack_events(&pool, "samvise", &[id]).await.unwrap();

        let out_samvise = poll_events(&pool, "samvise", None, &[], DEFAULT_POLL_TOP_K)
            .await
            .unwrap();
        assert!(
            out_samvise.events.is_empty(),
            "acked event invisible to samvise"
        );
        let out_telegram = poll_events(&pool, "telegram-bot", None, &[], DEFAULT_POLL_TOP_K)
            .await
            .unwrap();
        assert_eq!(
            out_telegram.events.len(),
            1,
            "still visible to telegram-bot"
        );
    }

    #[tokio::test]
    async fn poll_events_has_more_when_results_exceed_top_k() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        for _ in 0..3 {
            insert_event(
                &pool,
                EventKind::StructureApplied,
                None,
                None,
                &serde_json::Value::Null,
            )
            .await
            .unwrap();
        }
        let out = poll_events(&pool, "samvise", None, &[], 2).await.unwrap();
        assert_eq!(out.events.len(), 2);
        assert!(out.has_more);
    }

    #[tokio::test]
    async fn ack_events_returns_unknown_ids_for_missing_rows() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        let id = insert_event(
            &pool,
            EventKind::StructureApplied,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();
        let out = ack_events(&pool, "samvise", &[id, 999_999_999])
            .await
            .unwrap();
        assert_eq!(out.acked, 1);
        assert_eq!(out.unknown, vec![999_999_999]);
    }

    #[tokio::test]
    async fn ack_events_is_idempotent_at_the_consumer_level() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        let id = insert_event(
            &pool,
            EventKind::StructureApplied,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();
        let first = ack_events(&pool, "samvise", &[id]).await.unwrap();
        let second = ack_events(&pool, "samvise", &[id]).await.unwrap();
        assert_eq!(first.acked, 1);
        assert_eq!(second.acked, 1);
        assert!(second.unknown.is_empty());
        let out = poll_events(&pool, "samvise", None, &[], DEFAULT_POLL_TOP_K)
            .await
            .unwrap();
        assert!(out.events.is_empty());
    }

    /// Regression guard for the `SQLITE_BUSY_SNAPSHOT` deadlock that broke
    /// `events_ack` in production (v1.5.0). The pre-fix body ran
    /// `SELECT`-then-`UPDATE` inside a deferred transaction: the `SELECT`
    /// pinned a read snapshot, and if another connection committed a write
    /// before the `UPDATE` could upgrade to the write lock, `SQLite` rejected
    /// the upgrade with `(code: 5) database is locked` — a conflict
    /// `busy_timeout` cannot retry, so it failed instantly.
    ///
    /// This forces that interleaving deterministically: a second connection
    /// holds the write lock (`BEGIN IMMEDIATE` + an uncommitted write), we
    /// launch the ack (which — pre-fix — takes its read snapshot and then
    /// parks waiting for the lock), then commit the blocker so the WAL
    /// advances past the ack's snapshot. A read-first ack wakes into a stale
    /// snapshot and fails; the write-first body takes its snapshot only when
    /// it finally acquires the write lock, so it must succeed. The pool's
    /// default capacity (10) lets the blocker and the ack hold distinct
    /// connections concurrently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ack_events_survives_write_committed_after_it_started() {
        let pool = fresh_pool().await;
        register_dummy_consumer(&pool, "samvise").await;
        register_dummy_consumer(&pool, "poller").await;
        let id = insert_event(
            &pool,
            EventKind::StructureApplied,
            None,
            None,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();

        // Blocker: hold the write lock on a distinct connection with an
        // uncommitted write, so the ack that follows cannot take the write
        // lock until we commit — and that commit advances the WAL.
        let mut blocker = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("UPDATE consumers SET last_seen_at = ? WHERE consumer_id = ?")
            .bind(chrono::Utc::now().to_rfc3339())
            .bind("poller")
            .execute(&mut *blocker)
            .await
            .unwrap();

        // Launch the ack while the write lock is held. Pre-fix it reads its
        // snapshot now and parks on the UPDATE; post-fix it parks on the
        // UPDATE with no snapshot taken yet.
        let ack_pool = pool.clone();
        let ids = vec![id];
        let ack = tokio::spawn(async move { ack_events(&ack_pool, "samvise", &ids).await });

        // Let the ack task run its SELECT (pre-fix) and block on the UPDATE.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        // Commit the blocker: releases the write lock AND advances the WAL
        // past any snapshot the ack pinned before this point.
        sqlx::query("COMMIT").execute(&mut *blocker).await.unwrap();
        drop(blocker);

        let out = ack
            .await
            .unwrap()
            .expect("ack must not fail with a stale-snapshot lock error");
        assert_eq!(out.acked, 1);
        assert!(out.unknown.is_empty());

        // And the stamp actually landed: nothing pending for the consumer.
        let pending = poll_events(&pool, "samvise", None, &[], DEFAULT_POLL_TOP_K)
            .await
            .unwrap();
        assert!(pending.events.is_empty());
    }

    #[tokio::test]
    async fn find_recent_event_distinguishes_kinds() {
        let pool = fresh_pool().await;
        let fact_id = "018f1234-5678-7abc-9def-0123456789ae";
        insert_event(
            &pool,
            EventKind::DedupProposed,
            Some("alice"),
            Some(fact_id),
            &serde_json::Value::Null,
        )
        .await
        .expect("insert");
        // Same fact, different kind ⇒ probe must return false.
        let seen = find_recent_event_for(
            &pool,
            EventKind::StructureApplied,
            fact_id,
            chrono::Duration::days(30),
        )
        .await
        .expect("probe");
        assert!(!seen);
    }
}
