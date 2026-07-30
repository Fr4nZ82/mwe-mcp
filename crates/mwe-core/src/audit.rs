// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tool-execution audit trail — wraps the `tool_executions` table
//! (engine DB & migrations).
//!
//! ## Why this module exists
//!
//! Every MCP-exposed tool call gets one row in `tool_executions` so the
//! operator can later answer "who called what, when, with what cost,
//! did it fail" — without the audit row ever leaking back into a
//! consumer agent's context (see [identity and ACL](../../../docs/concepts/identity-and-acl.md)).
//!
//! Writes happen at the dispatcher boundary in
//! `mwe-mcp-server::mcp::audit`, on every call regardless of success.
//! Reads back out via [`search`], which backs the `tool_log_search` MCP
//! tool.
//!
//! ## Privacy invariants
//!
//! - `args_hash` is the SHA-256 of the tool's argument JSON — never the
//!   raw payload. The hash gives an audit reader the ability to spot
//!   duplicate calls without exposing PII.
//! - `result_summary` is short, human-readable, decided per-tool by the
//!   dispatcher (never the unbounded tool response body).
//! - `error` is the error *class* (`invalid_input`, `unauthorized`, …),
//!   never a stack trace or message that might quote payload bytes.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Errors raised by the audit layer.
#[derive(Debug, Error)]
pub enum AuditError {
    /// Underlying SQL failure (insert / select).
    #[error("audit db: {0}")]
    Db(#[from] sqlx::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, AuditError>;

/// Outcome flag stored in the `error` column: `None` ⇒ row was a
/// successful call, `Some` ⇒ error class string.
pub type ErrorClass = Option<String>;

/// One write request from the dispatcher boundary. Field shape matches
/// engine DB & migrations one-to-one.
#[derive(Debug, Clone)]
pub struct ToolExecutionInput<'a> {
    /// Stable MCP tool name (e.g. `wiki_ingest_message`). Use the
    /// canonical wire string, never a translated label.
    pub tool_name: &'a str,
    /// Validated `sender_id` from the caller's JWT.
    pub sender_id: &'a str,
    /// `device_label` claim — always present in our JWT shape.
    pub device_label: &'a str,
    /// `rate_limit_id` claim. Optional because not every code path
    /// passes one through (the dispatcher does, but tests / library
    /// users may not).
    pub rate_limit_id: Option<&'a str>,
    /// SHA-256 hex of the argument JSON body. Lowercase, no `0x`.
    pub args_hash: Option<&'a str>,
    /// Short human-readable summary of the result. Bounded length is
    /// the caller's responsibility (the dispatcher trims to ~256 chars
    /// before this hits SQL).
    pub result_summary: Option<&'a str>,
    /// Wall-clock latency of the call, milliseconds.
    pub latency_ms: i64,
    /// Estimated EUR cost (LLM-backed paths fill this; others leave it).
    pub cost_estimate: Option<f64>,
    /// Error class string, or `None` on success.
    pub error: Option<&'a str>,
}

/// Row shape returned by [`search`]. Mirrors `tool_executions` 1:1
/// (camelCase remapped for JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRow {
    /// ISO 8601 timestamp the dispatcher stamped at write time.
    pub timestamp: String,
    /// Tool name (`wiki_ingest_message`, `wiki_search`, ...).
    pub tool_name: String,
    /// `sender_id` derived from the call's JWT.
    pub sender_id: String,
    /// `device_label` from the JWT — useful for distinguishing a
    /// `dashboard-session` cookie from an MCP token.
    pub device_label: String,
    /// `rate_limit_id` from the JWT, if any.
    pub rate_limit_id: Option<String>,
    /// SHA-256 hash of the argument payload (lowercase hex).
    pub args_hash: Option<String>,
    /// Short result summary written by the dispatcher.
    pub result_summary: Option<String>,
    /// Call latency in milliseconds.
    pub latency_ms: i64,
    /// Estimated cost in EUR (LLM paths only).
    pub cost_estimate: Option<f64>,
    /// Error class on failure, `None` on success.
    pub error: Option<String>,
}

/// Insert one audit row. Returns the auto-increment id mostly for tests
/// — the production dispatcher discards it.
///
/// # Errors
///
/// - [`AuditError::Db`] for any SQL failure.
pub async fn record(pool: &SqlitePool, input: &ToolExecutionInput<'_>) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO tool_executions
             (timestamp, tool_name, sender_id, device_label, rate_limit_id,
              args_hash, result_summary, latency_ms, cost_estimate, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&now)
    .bind(input.tool_name)
    .bind(input.sender_id)
    .bind(input.device_label)
    .bind(input.rate_limit_id)
    .bind(input.args_hash)
    .bind(input.result_summary)
    .bind(input.latency_ms)
    .bind(input.cost_estimate)
    .bind(input.error)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Search filters for [`search`].
///
/// Every field is optional + AND-combined. Matches the input shape of
/// [`tool_log_search` in the tool reference](../../../docs/protocol/tool-reference.md).
#[derive(Debug, Default, Clone)]
pub struct SearchFilters {
    /// Constrain to one sender.
    pub sender_id_filter: Option<String>,
    /// Constrain to one tool name.
    pub tool_name_filter: Option<String>,
    /// Inclusive lower bound on `timestamp` (ISO 8601).
    pub date_from: Option<String>,
    /// Inclusive upper bound on `timestamp` (ISO 8601).
    pub date_to: Option<String>,
    /// `"success"` ⇒ `error IS NULL`; `"error"` ⇒ `error IS NOT NULL`;
    /// `None` ⇒ no filter.
    pub result_status: Option<ResultStatus>,
    /// Page size. Defaults to [`DEFAULT_SEARCH_TOP_K`], capped at
    /// [`MAX_SEARCH_TOP_K`].
    pub top_k: Option<i64>,
}

/// Wire enum for the `result_status` filter — matches the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    /// Successful calls only (`error IS NULL`).
    Success,
    /// Failed calls only (`error IS NOT NULL`).
    Error,
}

/// Default page size for [`search`] — matches the spec.
pub const DEFAULT_SEARCH_TOP_K: i64 = 50;
/// Maximum page size for [`search`] — matches the spec.
pub const MAX_SEARCH_TOP_K: i64 = 500;

type SearchTuple = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<f64>,
    Option<String>,
);

/// Query the audit log.
///
/// Rows are returned newest-first; pagination uses the same simple
/// "filters + `top_k`" model as the rest of the MVP tools (no opaque
/// cursors).
///
/// # Errors
///
/// - [`AuditError::Db`] for any SQL failure.
pub async fn search(pool: &SqlitePool, filters: &SearchFilters) -> Result<Vec<ToolExecutionRow>> {
    let limit = filters
        .top_k
        .unwrap_or(DEFAULT_SEARCH_TOP_K)
        .clamp(1, MAX_SEARCH_TOP_K);

    let mut sql = String::from(
        "SELECT timestamp, tool_name, sender_id, device_label, rate_limit_id,
                args_hash, result_summary, latency_ms, cost_estimate, error
           FROM tool_executions",
    );
    let mut where_clauses: Vec<&'static str> = Vec::new();
    if filters.sender_id_filter.is_some() {
        where_clauses.push("sender_id = ?");
    }
    if filters.tool_name_filter.is_some() {
        where_clauses.push("tool_name = ?");
    }
    if filters.date_from.is_some() {
        where_clauses.push("timestamp >= ?");
    }
    if filters.date_to.is_some() {
        where_clauses.push("timestamp <= ?");
    }
    match filters.result_status {
        Some(ResultStatus::Success) => where_clauses.push("error IS NULL"),
        Some(ResultStatus::Error) => where_clauses.push("error IS NOT NULL"),
        None => {},
    }
    if !where_clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY timestamp DESC, id DESC LIMIT ?");

    let mut query = sqlx::query_as::<_, SearchTuple>(&sql);
    if let Some(s) = &filters.sender_id_filter {
        query = query.bind(s);
    }
    if let Some(t) = &filters.tool_name_filter {
        query = query.bind(t);
    }
    if let Some(d) = &filters.date_from {
        query = query.bind(d);
    }
    if let Some(d) = &filters.date_to {
        query = query.bind(d);
    }
    let rows: Vec<SearchTuple> = query.bind(limit).fetch_all(pool).await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                timestamp,
                tool_name,
                sender_id,
                device_label,
                rate_limit_id,
                args_hash,
                result_summary,
                latency_ms,
                cost_estimate,
                error,
            )| {
                ToolExecutionRow {
                    timestamp,
                    tool_name,
                    sender_id,
                    device_label,
                    rate_limit_id,
                    args_hash,
                    result_summary,
                    latency_ms,
                    cost_estimate,
                    error,
                }
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_pool() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    #[tokio::test]
    async fn record_then_search_roundtrips() {
        let (_workdir, pool) = fresh_pool().await;
        record(
            &pool,
            &ToolExecutionInput {
                tool_name: "wiki_ingest_message",
                sender_id: "frodo",
                device_label: "claude-code",
                rate_limit_id: Some("default"),
                args_hash: Some("deadbeef"),
                result_summary: Some("captured"),
                latency_ms: 42,
                cost_estimate: Some(0.001),
                error: None,
            },
        )
        .await
        .unwrap();
        let rows = search(&pool, &SearchFilters::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool_name, "wiki_ingest_message");
        assert_eq!(rows[0].sender_id, "frodo");
        assert_eq!(rows[0].latency_ms, 42);
        assert!(rows[0].error.is_none());
    }

    #[tokio::test]
    async fn search_filters_by_sender_and_tool() {
        let (_workdir, pool) = fresh_pool().await;
        for (sender, tool, err) in [
            ("frodo", "wiki_ingest_message", None),
            ("frodo", "wiki_search", None),
            ("samvise", "wiki_ingest_message", Some("invalid_input")),
        ] {
            record(
                &pool,
                &ToolExecutionInput {
                    tool_name: tool,
                    sender_id: sender,
                    device_label: "x",
                    rate_limit_id: None,
                    args_hash: None,
                    result_summary: None,
                    latency_ms: 1,
                    cost_estimate: None,
                    error: err,
                },
            )
            .await
            .unwrap();
        }

        let by_sender = search(
            &pool,
            &SearchFilters {
                sender_id_filter: Some("frodo".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_sender.len(), 2);

        let by_tool = search(
            &pool,
            &SearchFilters {
                tool_name_filter: Some("wiki_search".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_tool.len(), 1);
        assert_eq!(by_tool[0].sender_id, "frodo");

        let errors = search(
            &pool,
            &SearchFilters {
                result_status: Some(ResultStatus::Error),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error.as_deref(), Some("invalid_input"));
    }

    #[tokio::test]
    async fn search_top_k_caps_at_max() {
        let (_workdir, pool) = fresh_pool().await;
        for i in 0..5 {
            record(
                &pool,
                &ToolExecutionInput {
                    tool_name: "wiki_search",
                    sender_id: "frodo",
                    device_label: "x",
                    rate_limit_id: None,
                    args_hash: None,
                    result_summary: Some(&format!("call {i}")),
                    latency_ms: 1,
                    cost_estimate: None,
                    error: None,
                },
            )
            .await
            .unwrap();
        }
        let rows = search(
            &pool,
            &SearchFilters {
                top_k: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
    }
}
