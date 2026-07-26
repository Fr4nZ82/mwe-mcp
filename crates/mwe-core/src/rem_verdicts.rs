//! Negative-verdict memo for the REM cycle's LLM confirmers.
//!
//! Every confirmer sweep in [`crate::rem`] asks a model one question
//! about a **stable** piece of the corpus — are these two facts the same
//! claim, has this page grown into two, does this fact belong in another
//! wiki — and the overwhelmingly common answer is "no". Without a memo
//! that "no" is re-bought every night at the same price, and the
//! per-cycle confirm caps are spent re-judging settled ground instead of
//! reaching the questions nobody has asked yet.
//!
//! This module is that memo. A row means: *this exact question, on this
//! exact content, judged by this exact model and prompt, already came
//! back negative.*
//!
//! ## What makes a key
//!
//! [`key`] hashes the model id together with the **rendered prompt** —
//! which already contains the prompt body and every piece of subject
//! content the model sees. That single input carries all three
//! invalidation axes for free:
//!
//! - edit a fact's text → different prompt → different key → re-asked;
//! - edit the prompt (bundled body or workdir override) → different key;
//! - switch the model in `mwe-mcp.config.yaml` → different key.
//!
//! A memo therefore can never outlive the thing it was a verdict about,
//! and no manual cache-busting version constant has to be maintained.
//!
//! The one case the rendered prompt is *too* sensitive is a prompt that
//! embeds a counter which moves on its own — the paragraph-split prompt
//! carries each fact's 30-day recall count, which changes without the
//! page changing. Those call sites hash a **canonical** rendering (same
//! template, counters bucketed into bands) instead; see
//! [`crate::rem`]'s `paragraph_split_memo_prompt`. The rule: the key is
//! derived from what would make the model answer *differently*, never
//! from noise.
//!
//! ## Only negatives
//!
//! A positive verdict mutates the corpus — facts merge, move, or close —
//! which changes the content the key is derived from. Positives
//! self-invalidate and are never recorded here.
//!
//! ## Bounded
//!
//! [`purge_older_than`] runs once at cycle start
//! (`RemPolicy::verdict_memo_ttl`, default 90 days) so a churning corpus
//! cannot grow the table without limit, and so every verdict eventually
//! gets a second opinion even when nothing about its subject moved.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Sub-job families that memoize their negative verdicts. The value is
/// the `kind` column, half of the primary key: two sweeps that happened
/// to render an identical prompt still keep separate memos.
pub mod kind {
    /// Revisor dedup pair confirm (`rem-dedup`).
    pub const DEDUP_PAIR: &str = "dedup_pair";
    /// Auto-promote paragraph → page split (`rem-promotions`).
    pub const PAGE_SPLIT: &str = "page_split";
    /// Auto-promote page → sub-wiki emergence (`rem-subwiki-emergence`).
    pub const SUBWIKI_EMERGENCE: &str = "subwiki_emergence";
    /// Page-merge confirm (`rem-merge`).
    pub const PAGE_MERGE: &str = "page_merge";
    /// Completion sweep confirm (`rem-completion`).
    pub const COMPLETION: &str = "completion";
    /// Contradiction sweep confirm (`rem-contradiction`).
    pub const CONTRADICTION: &str = "contradiction";
    /// Cross-wiki refile verdict (`rem-refile`).
    pub const REFILE: &str = "refile";
}

/// Derive the memo key for one confirmer question.
///
/// `rendered_prompt` is the exact string that would be sent to the model
/// (or its canonical, noise-free equivalent — see the module docs).
/// `model_id` comes from [`crate::llm::LlmBackend::model_id`], so moving
/// a slot to another model re-opens every question that slot had settled.
#[must_use]
pub fn key(model_id: &str, rendered_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_id.as_bytes());
    // Domain separator: a model id ending in the prompt's first bytes
    // must not collide with a shorter id and a longer prompt.
    hasher.update(b"\x00");
    hasher.update(rendered_prompt.as_bytes());
    hex::encode(hasher.finalize())
}

/// Has this exact question already come back negative?
///
/// # Errors
///
/// [`sqlx::Error`] on SQL infrastructure failure. Callers in
/// [`crate::rem`] convert via `RemError::Db`.
pub async fn is_settled(pool: &SqlitePool, kind: &str, key_hash: &str) -> sqlx::Result<bool> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rem_verdicts WHERE kind = ? AND key_hash = ?")
            .bind(kind)
            .bind(key_hash)
            .fetch_one(pool)
            .await?;
    Ok(n > 0)
}

/// Record a negative verdict.
///
/// Idempotent (`INSERT OR REPLACE`): re-recording refreshes `created_at`,
/// which is what we want — a verdict re-confirmed after a TTL expiry
/// earns another full TTL window rather than expiring again immediately.
///
/// `subject_ref` is a debugging breadcrumb only (a page path, a fact-id
/// pair); the lookup path never reads it.
///
/// # Errors
///
/// [`sqlx::Error`] on SQL infrastructure failure.
pub async fn record_negative(
    pool: &SqlitePool,
    kind: &str,
    key_hash: &str,
    subject_ref: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO rem_verdicts (kind, key_hash, subject_ref, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(kind)
    .bind(key_hash)
    .bind(subject_ref)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop memos older than `cutoff`. Returns how many rows went.
///
/// # Errors
///
/// [`sqlx::Error`] on SQL infrastructure failure.
pub async fn purge_older_than(pool: &SqlitePool, cutoff: DateTime<Utc>) -> sqlx::Result<u64> {
    let res = sqlx::query("DELETE FROM rem_verdicts WHERE created_at < ?")
        .bind(cutoff.to_rfc3339())
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Live row count — surfaced on the cycle report so an operator can see
/// the memo working (and spot a key that churns when it should not).
///
/// # Errors
///
/// [`sqlx::Error`] on SQL infrastructure failure.
pub async fn count(pool: &SqlitePool) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM rem_verdicts")
        .fetch_one(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    /// The three invalidation axes: model, prompt body, subject content.
    /// Any of them moving must yield a different key.
    #[test]
    fn key_changes_with_model_and_prompt() {
        let a = key(
            "gemini-3-flash",
            "A: nato il 23 maggio\nB: è nato il 23 maggio",
        );
        let same = key(
            "gemini-3-flash",
            "A: nato il 23 maggio\nB: è nato il 23 maggio",
        );
        let other_model = key(
            "claude-opus-4-8",
            "A: nato il 23 maggio\nB: è nato il 23 maggio",
        );
        let other_text = key(
            "gemini-3-flash",
            "A: nato il 24 maggio\nB: è nato il 23 maggio",
        );
        assert_eq!(a, same, "same question + same model must memoize");
        assert_ne!(a, other_model, "switching model must re-open the question");
        assert_ne!(a, other_text, "editing a fact must re-open the question");
    }

    /// The domain separator: a longer model id must not be able to
    /// impersonate a shorter one whose prompt starts with the remainder.
    #[test]
    fn key_is_not_forgeable_by_concatenation() {
        assert_ne!(key("ab", "cd"), key("a", "bcd"));
    }

    #[tokio::test]
    async fn negative_verdict_round_trips_and_is_kind_scoped() {
        let pool = make_pool().await;
        let k = key("m", "prompt");

        assert!(!is_settled(&pool, kind::DEDUP_PAIR, &k).await.expect("miss"));
        record_negative(&pool, kind::DEDUP_PAIR, &k, "f-1 vs f-2")
            .await
            .expect("record");
        assert!(is_settled(&pool, kind::DEDUP_PAIR, &k).await.expect("hit"));

        // Same key, different sub-job family: still unasked.
        assert!(
            !is_settled(&pool, kind::PAGE_SPLIT, &k)
                .await
                .expect("kind scoped"),
            "a memo must not leak across sub-job families"
        );

        // Idempotent: re-recording the same verdict is not an error and
        // does not duplicate the row.
        record_negative(&pool, kind::DEDUP_PAIR, &k, "f-1 vs f-2")
            .await
            .expect("re-record");
        assert_eq!(count(&pool).await.expect("count"), 1);
    }

    #[tokio::test]
    async fn purge_drops_only_expired_rows() {
        let pool = make_pool().await;
        // Plant one aged row directly (record_negative always stamps now).
        sqlx::query(
            "INSERT INTO rem_verdicts (kind, key_hash, subject_ref, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(kind::DEDUP_PAIR)
        .bind("stale")
        .bind("old pair")
        .bind((Utc::now() - chrono::Duration::days(120)).to_rfc3339())
        .execute(&pool)
        .await
        .expect("plant");
        record_negative(&pool, kind::DEDUP_PAIR, "fresh", "new pair")
            .await
            .expect("record");

        let purged = purge_older_than(&pool, Utc::now() - chrono::Duration::days(90))
            .await
            .expect("purge");
        assert_eq!(purged, 1, "only the aged row goes");
        assert!(
            is_settled(&pool, kind::DEDUP_PAIR, "fresh")
                .await
                .expect("fresh survives")
        );
        assert!(
            !is_settled(&pool, kind::DEDUP_PAIR, "stale")
                .await
                .expect("stale gone"),
            "an expired verdict must be re-asked, not silently kept"
        );
    }
}
