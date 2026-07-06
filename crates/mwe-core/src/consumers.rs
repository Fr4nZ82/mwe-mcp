// SPDX-License-Identifier: AGPL-3.0-or-later
//! Consumer registry — wraps the `consumers` table (migration 0016).
//!
//! ## Why this module exists
//!
//! Consumers (orchestrators, bots, plugins) must register themselves
//! before they can drain the event queue, so the dispatcher can both
//! reject calls from unknown consumers and track per-consumer delivery
//! state in [`crate::events`] via the `wiki_events.acks` JSON map.
//!
//! Two surfaces sit on top of this module:
//!
//! - [`register`] is the write-side called by the `consumer_register`
//!   MCP tool. Idempotent on `consumer_id` so a consumer can re-register
//!   on startup without surfacing a 409.
//! - [`is_registered`] is the read-side called by the `events_poll` /
//!   `events_ack` handlers to gate access.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Errors raised by the consumers layer.
#[derive(Debug, Error)]
pub enum ConsumersError {
    /// Underlying SQL failure.
    #[error("consumers db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON serialisation failure (`kinds_subscribed` / `metadata` fields).
    #[error("consumers json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ConsumersError>;

/// Request shape for [`register`].
///
/// Mirrors the [`consumer_register` tool reference](../../../docs/protocol/tool-reference.md).
#[derive(Debug, Clone)]
pub struct RegisterRequest<'a> {
    /// Stable consumer id, chosen by the consumer.
    pub consumer_id: &'a str,
    /// Human-readable label (optional).
    pub display_name: Option<&'a str>,
    /// Webhook URL for push delivery (optional; pull-only when absent).
    pub callback_url: Option<&'a str>,
    /// Subscribed event kinds (empty / `None` ⇒ all kinds).
    pub kinds_subscribed: Option<Vec<String>>,
    /// Free-shape metadata stored verbatim as JSON.
    pub metadata: Option<serde_json::Value>,
    /// The consumer's own memory identity — the credential-less **system
    /// user** in `enrollment_users` that this (standard) consumer *is*.
    /// Materialises the consumer ↔ system-user binding of the diagonal
    /// identity model (migration 0029). `None` for smart consumers (they
    /// authenticate as their human owner, not a bot identity) and for
    /// callers that do not supply it; a `None` on refresh **preserves**
    /// any existing binding rather than clearing it.
    pub system_user_id: Option<&'a str>,
}

/// Outcome shape for [`register`].
///
/// Mirrors the spec output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterOutcome {
    /// `true` when this was a fresh registration; `false` for a refresh
    /// of an existing row (same `consumer_id`).
    pub fresh_registration: bool,
    /// Hex-encoded shared secret returned to the consumer.
    /// **Returned only on a fresh registration**; on a refresh the
    /// existing secret is preserved server-side and `None` here so the
    /// consumer cannot rotate by mistake.
    pub consumer_secret: Option<String>,
    /// ISO 8601 timestamp the row was first registered (preserved on
    /// refresh — `metadata.registered_at`).
    pub registered_at: String,
}

/// Insert (or refresh) a consumer registration.
///
/// On collision with an existing `consumer_id`, mutable fields
/// (`display_name`, `callback_url`, `kinds_subscribed`, `metadata`) are
/// overwritten with the new values; `consumer_secret` and `registered_at`
/// are preserved (they are part of the consumer's identity).
///
/// # Errors
///
/// - [`ConsumersError::Json`] when the optional `metadata` / kinds list
///   cannot be serialised.
/// - [`ConsumersError::Db`] for any SQL failure.
pub async fn register(pool: &SqlitePool, req: &RegisterRequest<'_>) -> Result<RegisterOutcome> {
    let kinds_json = match &req.kinds_subscribed {
        Some(v) if !v.is_empty() => Some(serde_json::to_string(v)?),
        _ => None,
    };
    let metadata_json = match &req.metadata {
        Some(v) if !v.is_null() => Some(serde_json::to_string(v)?),
        _ => None,
    };

    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT consumer_secret, registered_at FROM consumers WHERE consumer_id = ?",
    )
    .bind(req.consumer_id)
    .fetch_optional(pool)
    .await?;

    if let Some((_secret, registered_at)) = existing {
        sqlx::query(
            "UPDATE consumers
                SET display_name = ?, callback_url = ?, kinds_subscribed = ?, metadata = ?,
                    system_user_id = COALESCE(?, system_user_id)
              WHERE consumer_id = ?",
        )
        .bind(req.display_name)
        .bind(req.callback_url)
        .bind(&kinds_json)
        .bind(&metadata_json)
        .bind(req.system_user_id)
        .bind(req.consumer_id)
        .execute(pool)
        .await?;
        tracing::info!(
            consumer_id = req.consumer_id,
            fresh = false,
            "consumer: registered"
        );
        return Ok(RegisterOutcome {
            fresh_registration: false,
            consumer_secret: None,
            registered_at,
        });
    }

    let mut secret_bytes = [0u8; 32];
    getrandom::getrandom(&mut secret_bytes).map_err(|e| {
        // OS RNG failures are essentially never recoverable; surface as
        // a generic SQL error class for the dispatcher to log + 500.
        sqlx::Error::Protocol(format!("OS RNG unavailable: {e}"))
    })?;
    let secret = hex::encode(secret_bytes);
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO consumers
             (consumer_id, display_name, callback_url, kinds_subscribed, metadata,
              consumer_secret, registered_at, last_seen_at, system_user_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?)",
    )
    .bind(req.consumer_id)
    .bind(req.display_name)
    .bind(req.callback_url)
    .bind(&kinds_json)
    .bind(&metadata_json)
    .bind(&secret)
    .bind(&now)
    .bind(req.system_user_id)
    .execute(pool)
    .await?;
    tracing::info!(
        consumer_id = req.consumer_id,
        fresh = true,
        "consumer: registered"
    );

    Ok(RegisterOutcome {
        fresh_registration: true,
        consumer_secret: Some(secret),
        registered_at: now,
    })
}

/// Read-side guard for `events_poll` / `events_ack`: returns `true`
/// when `consumer_id` has a row in the registry.
///
/// # Errors
///
/// - [`ConsumersError::Db`] for any SQL failure.
pub async fn is_registered(pool: &SqlitePool, consumer_id: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM consumers WHERE consumer_id = ?")
        .bind(consumer_id)
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

/// Resolve a consumer's bound **system user** from its `consumer_id`.
///
/// The credential-less system user bound at registration (the diagonal
/// identity model, migration 0029) is the consumer's own memory identity.
/// Returns `None` when the consumer is unknown or carries no binding (e.g. a
/// smart consumer that authenticates as its human owner). Used by ingest
/// behaviour-rule routing to locate the consumer's own wiki.
///
/// # Errors
///
/// - [`ConsumersError::Db`] for any SQL failure.
pub async fn system_user_for(pool: &SqlitePool, consumer_id: &str) -> Result<Option<String>> {
    let bound: Option<Option<String>> =
        sqlx::query_scalar("SELECT system_user_id FROM consumers WHERE consumer_id = ?")
            .bind(consumer_id)
            .fetch_optional(pool)
            .await?;
    Ok(bound.flatten())
}

/// Establish a standard consumer's agent identity **from its token**, idempotently.
///
/// Called from the MCP auth middleware the moment a `standard` token connects: it
/// (1) binds the consumer to the bot's own system-user (so [`system_user_for`]
/// resolves the agent wiki for behaviour-rule + agent-authored-memory routing) and
/// (2) stamps `enrollment_users.is_agent` — WITHOUT the consumer having to call
/// `consumer_register` separately (the setup step a conversational bridge skips and
/// which left agents like Hermes unbound). This is the un-deferred "explicit
/// `is_system` marker" of the diagonal identity model
/// (`docs/concepts/identity-and-acl.md` §1.5): the agent is wired by *connecting*,
/// not by a registration that can be forgotten.
///
/// Guarded: when the binding is already correct it is a single read and returns, so
/// the hot path pays one indexed `SELECT` after the first connection. Best-effort by
/// contract — a memory feature must never break authentication, so the caller logs
/// and proceeds on error.
///
/// # Errors
/// - [`ConsumersError::Db`] for any SQL failure.
pub async fn ensure_agent_identity(
    pool: &SqlitePool,
    consumer_id: &str,
    system_user: &str,
) -> Result<()> {
    // Guard: once the binding is in place, skip the writes.
    if system_user_for(pool, consumer_id).await?.as_deref() == Some(system_user) {
        return Ok(());
    }
    // Bind consumer ↔ system-user (idempotent on consumer_id; mints/keeps the
    // events secret). This does `consumer_register`'s job, driven by the token.
    register(
        pool,
        &RegisterRequest {
            consumer_id,
            display_name: None,
            callback_url: None,
            kinds_subscribed: None,
            metadata: None,
            system_user_id: Some(system_user),
        },
    )
    .await?;
    // Stamp the explicit identity marker (the gate/security discriminator).
    crate::enrollment::mark_agent(pool, system_user).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn fresh_pool() -> SqlitePool {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        db::open_or_init(dir.path()).await.expect("db open")
    }

    #[tokio::test]
    async fn register_fresh_returns_secret_and_marks_registered() {
        let pool = fresh_pool().await;
        let out = register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: Some("Samvise"),
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        assert!(out.fresh_registration);
        let secret = out.consumer_secret.unwrap();
        assert_eq!(secret.len(), 64, "32 random bytes ⇒ 64 hex chars");
        assert!(is_registered(&pool, "samvise-prod").await.unwrap());
    }

    #[tokio::test]
    async fn register_refresh_preserves_secret_and_first_registered_at() {
        let pool = fresh_pool().await;
        let first = register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: Some("Samvise (v1)"),
                callback_url: None,
                kinds_subscribed: Some(vec!["dedup_proposed".into()]),
                metadata: None,
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        // Wait a millisecond so the would-be new registered_at differs.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let second = register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: Some("Samvise (v2)"),
                callback_url: Some("https://example/webhook"),
                kinds_subscribed: Some(vec!["dedup_proposed".into(), "structure_applied".into()]),
                metadata: Some(serde_json::json!({"version": "2.0"})),
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        assert!(!second.fresh_registration);
        assert!(second.consumer_secret.is_none(), "secret hidden on refresh");
        assert_eq!(second.registered_at, first.registered_at);

        // The mutable fields propagated.
        let (display, callback, kinds): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT display_name, callback_url, kinds_subscribed FROM consumers WHERE consumer_id = ?",
            )
            .bind("samvise-prod")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(display.as_deref(), Some("Samvise (v2)"));
        assert_eq!(callback.as_deref(), Some("https://example/webhook"));
        assert!(kinds.unwrap().contains("structure_applied"));
    }

    #[tokio::test]
    async fn is_registered_distinguishes_known_and_unknown() {
        let pool = fresh_pool().await;
        assert!(!is_registered(&pool, "anyone").await.unwrap());
        register(
            &pool,
            &RegisterRequest {
                consumer_id: "anyone",
                display_name: None,
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        assert!(is_registered(&pool, "anyone").await.unwrap());
        assert!(!is_registered(&pool, "stranger").await.unwrap());
    }

    #[tokio::test]
    async fn register_records_and_preserves_system_user_binding() {
        let pool = fresh_pool().await;
        // The bound system user must exist first (FK:
        // consumers.system_user_id → enrollment_users.user_id).
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('samvise-bot', 0)")
            .execute(&pool)
            .await
            .unwrap();

        // Fresh registration captures the binding.
        register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: Some("Samvise"),
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: Some("samvise-bot"),
            },
        )
        .await
        .unwrap();
        let bound: Option<String> =
            sqlx::query_scalar("SELECT system_user_id FROM consumers WHERE consumer_id = ?")
                .bind("samvise-prod")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bound.as_deref(), Some("samvise-bot"));

        // A refresh that omits the binding (None) must PRESERVE it (COALESCE),
        // not clear it.
        register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: Some("Samvise (v2)"),
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        let still: Option<String> =
            sqlx::query_scalar("SELECT system_user_id FROM consumers WHERE consumer_id = ?")
                .bind("samvise-prod")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            still.as_deref(),
            Some("samvise-bot"),
            "None on refresh must preserve the binding"
        );
    }

    #[tokio::test]
    async fn system_user_for_resolves_binding_and_handles_absence() {
        let pool = fresh_pool().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('samvise-bot', 0)")
            .execute(&pool)
            .await
            .unwrap();

        // Unknown consumer → None.
        assert_eq!(system_user_for(&pool, "nope").await.unwrap(), None);

        // Registered with a binding → resolves it.
        register(
            &pool,
            &RegisterRequest {
                consumer_id: "samvise-prod",
                display_name: None,
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: Some("samvise-bot"),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            system_user_for(&pool, "samvise-prod")
                .await
                .unwrap()
                .as_deref(),
            Some("samvise-bot")
        );

        // Registered without a binding (a smart consumer) → None.
        register(
            &pool,
            &RegisterRequest {
                consumer_id: "cc-laptop",
                display_name: None,
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(system_user_for(&pool, "cc-laptop").await.unwrap(), None);
    }
}
