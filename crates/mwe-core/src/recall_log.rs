// SPDX-License-Identifier: AGPL-3.0-or-later
//! The hindsight recall log + the judge-free miss records — the
//! detection floor of self-correcting REM.
//!
//! Two bounded side tables (migration `0058`):
//!
//! - **`recall_log`** — one lean row per LLM-routed ingest turn: the fact
//!   ids the recall block surfaced (flat + fresh + due-soon) and the
//!   navigated pages' workdir-relative source paths. Written best-effort
//!   at the end of the turn ([`crate::ingest`]); its only consumer today
//!   is the promotion-time detector below, which needs to look back at
//!   what a buffered capture's original turn surfaced (the buffer row
//!   carries the `recall_log_id` linkage).
//! - **`recall_misses`** — one row per **restated-known-fact miss**: the
//!   memory held fact X, that turn's recall did not surface X, and the
//!   user restated it — proven by the write-time dedup hit
//!   (`capture::best_dedup_candidate`), no LLM verdict anywhere.
//!   Recorded from both dedup surfaces: the direct capture path (checked
//!   in-memory at the end of the same turn) and the light dream's
//!   promotion fold (checked against the logged turn).
//!
//! Both tables are telemetry-class: every writer is best-effort (a miss
//! in the miss log is ironic, not fatal) and both are age-pruned on
//! write — resource caps, not semantic gates. The records feed the
//! repair stages of the group (classification, the gold-set-gated
//! re-file) and are the raw material for growing the recall-eval gold
//! set; nothing consumes them destructively today.

use sqlx::SqlitePool;

/// Retention of the per-turn log, in days. The promotion detector needs
/// it only as long as a capture can sit buffered (hours); 30 days keeps
/// a comfortable offline-analysis window without unbounded growth.
const RECALL_LOG_RETENTION_DAYS: i64 = 30;

/// Retention of the miss records, in days. They are the repair queue of
/// the later stages — kept longer than the log they reference.
const RECALL_MISS_RETENTION_DAYS: i64 = 90;

/// One `recall_log` row, decoded.
#[derive(Debug, Clone)]
pub struct LogRow {
    /// Row id — the linkage key buffered captures carry.
    pub log_id: i64,
    /// The turn's clock (ISO-8601).
    pub created_at: String,
    /// Sender served on that turn.
    pub sender_id: String,
    /// Fact ids the recall block surfaced (flat + fresh + due-soon).
    pub fact_ids: Vec<String>,
    /// Workdir-relative source paths of the navigated pages.
    pub page_paths: Vec<String>,
    /// The classifier's topic seeds of the turn — the query side of a
    /// faithful gate replay.
    pub topics: Vec<String>,
}

impl LogRow {
    /// Was this fact surfaced on the logged turn — as a hit, or as prose
    /// of a navigated page?
    #[must_use]
    pub fn surfaced(&self, fact_id: &str, source_path: &str) -> bool {
        self.fact_ids.iter().any(|id| id == fact_id)
            || self.page_paths.iter().any(|p| p == source_path)
    }
}

/// A detected restated-known-fact miss, ready to record.
#[derive(Debug)]
pub struct NewMiss<'a> {
    /// The turn's / detection clock (ISO-8601).
    pub created_at: &'a str,
    /// Sender whose turn restated the fact.
    pub sender_id: &'a str,
    /// The fact memory held but recall did not surface.
    pub fact_id: &'a str,
    /// Its home wiki at detection time.
    pub wiki_id: &'a str,
    /// Its home page at detection time (workdir-relative).
    pub source_path: &'a str,
    /// Which dedup surface proved the restatement.
    pub surface: MissSurface,
    /// The dedup score.
    pub similarity: f32,
    /// The user's restatement (the new capture's body).
    pub restated_text: &'a str,
    /// The turn's `recall_log` row, when known.
    pub log_id: Option<i64>,
    /// The turn's classifier topic seeds, carried onto the miss so the
    /// repair gate can replay the query as production gathered it.
    pub seed_topics: &'a [String],
}

/// The dedup surface a miss was detected on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissSurface {
    /// The direct capture path's write-time dedup (same turn, in-memory).
    Direct,
    /// The light dream's promotion fold (offline, via the logged turn).
    Promotion,
}

impl MissSurface {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Promotion => "promotion",
        }
    }
}

/// One `recall_misses` row, read back (admin surfaces + the repair
/// stages).
#[derive(Debug, Clone)]
pub struct MissRow {
    /// Row id.
    pub miss_id: i64,
    /// Detection clock.
    pub created_at: String,
    /// Sender whose turn restated the fact.
    pub sender_id: String,
    /// The fact recall failed to surface.
    pub fact_id: String,
    /// Its home wiki at detection time.
    pub wiki_id: String,
    /// Its home page at detection time.
    pub source_path: String,
    /// `direct` | `promotion`.
    pub surface: String,
    /// The dedup score.
    pub similarity: Option<f64>,
    /// The user's restatement.
    pub restated_text: String,
    /// The turn's log row, when known.
    pub log_id: Option<i64>,
    /// Lifecycle: `new` → `repaired` | `queued` | `discarded` | `stale`.
    pub status: String,
    /// Resolution anchor (receipt id or a short reason tag).
    pub resolution: Option<String>,
    /// The turn's classifier topic seeds.
    pub seed_topics: Vec<String>,
}

/// Record one turn's surfaced set; returns the new row's `log_id`.
/// Prunes rows older than the retention window in the same call.
///
/// # Errors
///
/// Underlying `sqlx` errors (callers treat the whole log as best-effort).
pub async fn record_turn(
    pool: &SqlitePool,
    sender_id: &str,
    now: &str,
    fact_ids: &[String],
    page_paths: &[String],
    topics: &[String],
) -> sqlx::Result<i64> {
    let fact_ids_json = serde_json::to_string(fact_ids).unwrap_or_else(|_| "[]".to_owned());
    let page_paths_json = serde_json::to_string(page_paths).unwrap_or_else(|_| "[]".to_owned());
    let topics_json = serde_json::to_string(topics).unwrap_or_else(|_| "[]".to_owned());
    let row = sqlx::query(
        "INSERT INTO recall_log (created_at, sender_id, fact_ids, page_paths, topics) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(now)
    .bind(sender_id)
    .bind(&fact_ids_json)
    .bind(&page_paths_json)
    .bind(&topics_json)
    .execute(pool)
    .await?;
    prune(pool, "recall_log", RECALL_LOG_RETENTION_DAYS).await?;
    Ok(row.last_insert_rowid())
}

/// Load one logged turn. `None` when pruned or never recorded.
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn find_log(pool: &SqlitePool, log_id: i64) -> sqlx::Result<Option<LogRow>> {
    let row: Option<(i64, String, String, String, String, String)> = sqlx::query_as(
        "SELECT log_id, created_at, sender_id, fact_ids, page_paths, topics FROM recall_log \
         WHERE log_id = ?",
    )
    .bind(log_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(log_id, created_at, sender_id, fact_ids, page_paths, topics)| LogRow {
            log_id,
            created_at,
            sender_id,
            fact_ids: serde_json::from_str(&fact_ids).unwrap_or_default(),
            page_paths: serde_json::from_str(&page_paths).unwrap_or_default(),
            topics: serde_json::from_str(&topics).unwrap_or_default(),
        },
    ))
}

/// Record one detected miss. Prunes aged rows in the same call.
///
/// # Errors
///
/// Underlying `sqlx` errors (callers treat the record as best-effort).
pub async fn record_miss(pool: &SqlitePool, miss: &NewMiss<'_>) -> sqlx::Result<()> {
    let seed_topics_json =
        serde_json::to_string(miss.seed_topics).unwrap_or_else(|_| "[]".to_owned());
    sqlx::query(
        "INSERT INTO recall_misses \
             (created_at, sender_id, fact_id, wiki_id, source_path, surface, similarity, \
              restated_text, log_id, seed_topics) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(miss.created_at)
    .bind(miss.sender_id)
    .bind(miss.fact_id)
    .bind(miss.wiki_id)
    .bind(miss.source_path)
    .bind(miss.surface.as_str())
    .bind(f64::from(miss.similarity))
    .bind(miss.restated_text)
    .bind(miss.log_id)
    .bind(&seed_topics_json)
    .execute(pool)
    .await?;
    tracing::info!(
        fact_id = miss.fact_id,
        wiki_id = miss.wiki_id,
        surface = miss.surface.as_str(),
        similarity = miss.similarity,
        "recall miss: restated known fact was absent from its turn's recall"
    );
    prune(pool, "recall_misses", RECALL_MISS_RETENTION_DAYS).await?;
    Ok(())
}

/// Newest-first miss records (admin surfaces, the later repair stages).
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn recent_misses(pool: &SqlitePool, limit: usize) -> sqlx::Result<Vec<MissRow>> {
    fetch_misses(pool, "ORDER BY miss_id DESC", None, limit).await
}

/// Unprocessed (`status = 'new'`) misses, oldest first — the repair
/// sub-job's work queue.
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn pending_misses(pool: &SqlitePool, limit: usize) -> sqlx::Result<Vec<MissRow>> {
    fetch_misses(pool, "ORDER BY miss_id ASC", Some("new"), limit).await
}

/// Advance one miss's lifecycle (`repaired` | `queued` | `discarded` |
/// `stale`), with an optional resolution anchor (receipt id / reason tag).
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn set_miss_status(
    pool: &SqlitePool,
    miss_id: i64,
    status: &str,
    resolution: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE recall_misses SET status = ?, resolution = ? WHERE miss_id = ?")
        .bind(status)
        .bind(resolution)
        .bind(miss_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// How many misses (any status) were ever recorded for one fact — the
/// recurrence signal behind the operator-queue notice.
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn miss_count_for_fact(pool: &SqlitePool, fact_id: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM recall_misses WHERE fact_id = ?")
        .bind(fact_id)
        .fetch_one(pool)
        .await
}

async fn fetch_misses(
    pool: &SqlitePool,
    order: &str,
    status: Option<&str>,
    limit: usize,
) -> sqlx::Result<Vec<MissRow>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        miss_id: i64,
        created_at: String,
        sender_id: String,
        fact_id: String,
        wiki_id: String,
        source_path: String,
        surface: String,
        similarity: Option<f64>,
        restated_text: String,
        log_id: Option<i64>,
        status: String,
        resolution: Option<String>,
        seed_topics: String,
    }
    // `order` comes from the two literal call sites above, never caller
    // input — the interpolation is injection-safe by construction.
    let where_clause = if status.is_some() {
        "WHERE status = ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT miss_id, created_at, sender_id, fact_id, wiki_id, source_path, surface, \
                similarity, restated_text, log_id, status, resolution, seed_topics \
           FROM recall_misses {where_clause} {order} LIMIT ?"
    );
    let mut q = sqlx::query_as::<_, Row>(&sql);
    if let Some(s) = status {
        q = q.bind(s.to_owned());
    }
    let rows: Vec<Row> = q
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| MissRow {
            miss_id: r.miss_id,
            created_at: r.created_at,
            sender_id: r.sender_id,
            fact_id: r.fact_id,
            wiki_id: r.wiki_id,
            source_path: r.source_path,
            surface: r.surface,
            similarity: r.similarity,
            restated_text: r.restated_text,
            log_id: r.log_id,
            status: r.status,
            resolution: r.resolution,
            seed_topics: serde_json::from_str(&r.seed_topics).unwrap_or_default(),
        })
        .collect())
}

/// Age-based prune shared by both tables — a resource cap, not a gate.
async fn prune(pool: &SqlitePool, table: &str, retention_days: i64) -> sqlx::Result<()> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
    // `table` comes from the two literal call sites above, never caller
    // input — the interpolation is injection-safe by construction.
    sqlx::query(&format!("DELETE FROM {table} WHERE created_at < ?"))
        .bind(&cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        std::mem::forget(dir); // keep the tempdir alive for the pool's life
        pool
    }

    #[tokio::test]
    async fn record_and_lookup_roundtrip_with_surfaced_membership() {
        let pool = pool().await;
        let log_id = record_turn(
            &pool,
            "franz",
            "2026-07-05T10:00:00+00:00",
            &["fact-a".to_owned(), "fact-b".to_owned()],
            &["wikis/franz/index.md".to_owned()],
            &["cucina".to_owned()],
        )
        .await
        .expect("record");

        let row = find_log(&pool, log_id).await.unwrap().expect("row");
        assert_eq!(row.sender_id, "franz");
        assert!(row.surfaced("fact-a", "wikis/x/y.md"), "hit by fact id");
        assert!(
            row.surfaced("fact-z", "wikis/franz/index.md"),
            "hit by navigated page"
        );
        assert!(!row.surfaced("fact-z", "wikis/x/y.md"), "neither → miss");
        assert!(find_log(&pool, log_id + 999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn misses_record_and_read_newest_first() {
        let pool = pool().await;
        for (i, fid) in ["f-1", "f-2"].iter().enumerate() {
            record_miss(
                &pool,
                &NewMiss {
                    created_at: &format!("2026-07-05T10:0{i}:00+00:00"),
                    sender_id: "franz",
                    fact_id: fid,
                    wiki_id: "franz",
                    source_path: "wikis/franz/index.md",
                    surface: MissSurface::Direct,
                    similarity: 0.91,
                    restated_text: "il colore preferito è l'indaco",
                    log_id: None,
                    seed_topics: &[],
                },
            )
            .await
            .expect("record miss");
        }
        let rows = recent_misses(&pool, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fact_id, "f-2", "newest first");
        assert_eq!(rows[1].surface, "direct");
    }

    #[tokio::test]
    async fn writes_prune_rows_past_retention() {
        let pool = pool().await;
        // Plant an over-aged log row and miss row directly.
        sqlx::query(
            "INSERT INTO recall_log (created_at, sender_id, fact_ids, page_paths) \
             VALUES ('2020-01-01T00:00:00+00:00', 'franz', '[]', '[]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO recall_misses (created_at, sender_id, fact_id, wiki_id, source_path, \
             surface, restated_text) \
             VALUES ('2020-01-01T00:00:00+00:00', 'franz', 'f', 'w', 'p', 'direct', 't')",
        )
        .execute(&pool)
        .await
        .unwrap();

        record_turn(&pool, "franz", "2026-07-05T10:00:00+00:00", &[], &[], &[])
            .await
            .unwrap();
        record_miss(
            &pool,
            &NewMiss {
                created_at: "2026-07-05T10:00:00+00:00",
                sender_id: "franz",
                fact_id: "f-new",
                wiki_id: "w",
                source_path: "p",
                surface: MissSurface::Promotion,
                similarity: 0.9,
                restated_text: "t",
                log_id: None,
                seed_topics: &[],
            },
        )
        .await
        .unwrap();

        let logs: i64 = sqlx::query_scalar("SELECT count(*) FROM recall_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        let misses: i64 = sqlx::query_scalar("SELECT count(*) FROM recall_misses")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(logs, 1, "the 2020 log row is pruned");
        assert_eq!(misses, 1, "the 2020 miss row is pruned");
    }
}
