//! Cross-consumer recent window — the thread of discourse follows the user
//! (roadmap group 43).
//!
//! A bounded, TTL'd per-user serving buffer of the exchanges the per-turn
//! ingest already receives (the user's text plus the group-27 assistant
//! pass). Serving it back per turn — minus the requesting surface — gives
//! every consumer the user's live thread from their OTHER channels: say a
//! thing to the voice assistant, walk to Telegram, and the conversation
//! continues.
//!
//! **Not a transcript store** (the 43-P principle restatement): hard cap
//! per user AND a short TTL, both enforced in the write path; the rows are
//! never indexed, never embedded, never REM-processed. The TTL is short by
//! design — this buffer serves the *thread of discourse*, not history: a
//! thread is live on the scale of minutes to hours, and anything older has
//! either sedimented into facts through the ordinary ingest or wasn't
//! worth keeping.

use sqlx::SqlitePool;

use crate::ingest::MessageRole;

/// One buffered exchange, as served back to a consumer.
#[derive(Debug, Clone)]
pub struct RecentExchange {
    /// Consumer that carried the exchange ('' when the token had none).
    pub consumer_id: String,
    /// Surface label the consumer chose (`metadata.channel`; '' when unset).
    pub channel: String,
    /// Who spoke: the user or the consumer's agent.
    pub author: MessageRole,
    /// The utterance text (stored verbatim; capped at render time).
    pub text: String,
    /// ISO-8601 instant of the turn's semantic clock.
    pub occurred_at: String,
}

/// Record one exchange and enforce the per-user bounds in the same call.
///
/// Rows older than `ttl_hours` and rows beyond the newest `cap` are deleted
/// on the way in, so the buffer can never grow past its contract even if
/// nothing ever reads it. A `cap` of 0 disables the buffer (nothing is
/// written).
#[allow(clippy::too_many_arguments)]
pub async fn record_exchange(
    pool: &SqlitePool,
    user_id: &str,
    consumer_id: &str,
    channel: &str,
    author: MessageRole,
    text: &str,
    occurred_at: chrono::DateTime<chrono::Utc>,
    cap: usize,
    ttl_hours: u32,
) -> Result<(), sqlx::Error> {
    if cap == 0 || text.trim().is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO recent_exchanges (user_id, consumer_id, channel, author, text, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(consumer_id)
    .bind(channel)
    .bind(match author {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    })
    .bind(text)
    .bind(occurred_at.to_rfc3339())
    .execute(pool)
    .await?;
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(i64::from(ttl_hours))).to_rfc3339();
    sqlx::query("DELETE FROM recent_exchanges WHERE user_id = ? AND occurred_at < ?")
        .bind(user_id)
        .bind(&cutoff)
        .execute(pool)
        .await?;
    sqlx::query(
        "DELETE FROM recent_exchanges WHERE user_id = ? AND id NOT IN \
         (SELECT id FROM recent_exchanges WHERE user_id = ? ORDER BY id DESC LIMIT ?)",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(i64::try_from(cap).unwrap_or(i64::MAX))
    .execute(pool)
    .await?;
    Ok(())
}

/// Which surfaces of the user's window a requester is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceFilter {
    /// The requester holds its own live context: hide its surface — the
    /// (consumer, channel) pair when a channel was declared, the whole
    /// consumer otherwise — so the served section stays purely additive.
    ExcludeRequester,
    /// The requester carried NO local context (a reborn/blank session, or
    /// a consumer that keeps no window at all): serve every surface, its
    /// own included — there is nothing local a served thread could
    /// duplicate, and its own channel's tail is exactly the thread the
    /// user is continuing (fresh-session resume, 43j / hermes#43008).
    IncludeRequester,
}

/// The user's live window, oldest first, filtered per [`SurfaceFilter`].
///
/// Self-echo exclusion (the `ExcludeRequester` arm): when the requester
/// declared a `channel`, only its own (consumer, channel) pair is excluded —
/// sibling channels of the same consumer stay visible (the hermes case: one
/// token, many chats). Without a channel the whole consumer is excluded —
/// conservative, no self-echo at the cost of sibling-channel visibility.
pub async fn fetch_window(
    pool: &SqlitePool,
    user_id: &str,
    requesting_consumer: &str,
    requesting_channel: Option<&str>,
    filter: SurfaceFilter,
    ttl_hours: u32,
    limit: usize,
) -> Result<Vec<RecentExchange>, sqlx::Error> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(i64::from(ttl_hours))).to_rfc3339();
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows: Vec<(String, String, String, String, String)> = match (filter, requesting_channel) {
        (SurfaceFilter::IncludeRequester, _) => {
            sqlx::query_as(
                "SELECT consumer_id, channel, author, text, occurred_at FROM recent_exchanges \
                 WHERE user_id = ? AND occurred_at >= ? \
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(&cutoff)
            .bind(limit)
            .fetch_all(pool)
            .await?
        },
        (SurfaceFilter::ExcludeRequester, Some(channel)) => {
            sqlx::query_as(
                "SELECT consumer_id, channel, author, text, occurred_at FROM recent_exchanges \
                 WHERE user_id = ? AND occurred_at >= ? \
                 AND NOT (consumer_id = ? AND channel = ?) \
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(&cutoff)
            .bind(requesting_consumer)
            .bind(channel)
            .bind(limit)
            .fetch_all(pool)
            .await?
        },
        (SurfaceFilter::ExcludeRequester, None) => {
            sqlx::query_as(
                "SELECT consumer_id, channel, author, text, occurred_at FROM recent_exchanges \
                 WHERE user_id = ? AND occurred_at >= ? AND consumer_id != ? \
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(&cutoff)
            .bind(requesting_consumer)
            .bind(limit)
            .fetch_all(pool)
            .await?
        },
    };
    let mut out: Vec<RecentExchange> = rows
        .into_iter()
        .map(
            |(consumer_id, channel, author, text, occurred_at)| RecentExchange {
                consumer_id,
                channel,
                author: if author == "assistant" {
                    MessageRole::Assistant
                } else {
                    MessageRole::User
                },
                text,
                occurred_at,
            },
        )
        .collect();
    out.reverse(); // DESC-limited fetch keeps the newest; serve oldest first
    Ok(out)
}

/// Human-readable age of an exchange relative to the turn's clock.
///
/// Rendered instead of wall-clock time so staleness stays legible to the
/// model ("2 min ago" reads as a live thread, "3 h ago" as a fading one).
#[must_use]
pub fn relative_age(occurred_at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(occurred_at) else {
        return "some time ago".to_owned();
    };
    let mins = (now - then.with_timezone(&chrono::Utc))
        .num_minutes()
        .max(0);
    match mins {
        0..=1 => "just now".to_owned(),
        2..=59 => format!("{mins} min ago"),
        60..=1439 => format!("{} h ago", mins / 60),
        _ => format!("{} d ago", mins / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn cap_and_ttl_bound_the_buffer() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        for i in 0..5 {
            record_exchange(
                &pool,
                "anna",
                "bot",
                "telegram",
                MessageRole::User,
                &format!("m{i}"),
                now,
                3,
                4,
            )
            .await
            .unwrap();
        }
        // Cap 3: only the newest three survive.
        let got = fetch_window(
            &pool,
            "anna",
            "other",
            None,
            SurfaceFilter::ExcludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        let texts: Vec<_> = got.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["m2", "m3", "m4"]);
        // An expired row is dropped on the next write.
        record_exchange(
            &pool,
            "anna",
            "bot",
            "telegram",
            MessageRole::User,
            "old",
            now - chrono::Duration::hours(9),
            8,
            4,
        )
        .await
        .unwrap();
        let got = fetch_window(
            &pool,
            "anna",
            "other",
            None,
            SurfaceFilter::ExcludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        assert!(got.iter().all(|e| e.text != "old"), "expired row served");
    }

    #[tokio::test]
    async fn self_echo_exclusion_by_surface_then_by_consumer() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        for (channel, text) in [("salotto", "dal salotto"), ("telegram", "dalla chat")] {
            record_exchange(
                &pool,
                "anna",
                "bot",
                channel,
                MessageRole::User,
                text,
                now,
                32,
                4,
            )
            .await
            .unwrap();
        }
        // Channel declared: only the same (consumer, channel) pair is hidden.
        let got = fetch_window(
            &pool,
            "anna",
            "bot",
            Some("telegram"),
            SurfaceFilter::ExcludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        let texts: Vec<_> = got.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["dal salotto"]);
        // No channel: the whole consumer is hidden.
        let got = fetch_window(
            &pool,
            "anna",
            "bot",
            None,
            SurfaceFilter::ExcludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        assert!(got.is_empty());
        // A requester with no local context is served every surface, its
        // own included (fresh-session resume, 43j).
        let got = fetch_window(
            &pool,
            "anna",
            "bot",
            Some("telegram"),
            SurfaceFilter::IncludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        assert_eq!(got.len(), 2);
        // Windows never cross users.
        let got = fetch_window(
            &pool,
            "beppe",
            "other",
            None,
            SurfaceFilter::ExcludeRequester,
            4,
            32,
        )
        .await
        .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn relative_age_buckets() {
        let now = chrono::Utc::now();
        let at = |mins: i64| (now - chrono::Duration::minutes(mins)).to_rfc3339();
        assert_eq!(relative_age(&at(0), now), "just now");
        assert_eq!(relative_age(&at(5), now), "5 min ago");
        assert_eq!(relative_age(&at(120), now), "2 h ago");
        assert_eq!(relative_age("garbage", now), "some time ago");
    }
}
