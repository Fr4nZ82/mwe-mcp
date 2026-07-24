// SPDX-License-Identifier: AGPL-3.0-or-later
//! `engine.db` access layer — connection pool + migrations.
//!
//! [`open_or_init`] opens (or creates) the sqlite file at
//! `<workdir>/engine.db`, applies the canonical pragmas
//! (`journal_mode=WAL`, `foreign_keys=ON`, plus a generous `busy_timeout`
//! for the brief contention window between writers and concurrent
//! readers; see
//! engine DB and migrations),
//! and runs every pending migration under
//! [`migrations/`](../../migrations/).
//!
//! The 15 migration files (see `0001_*` through `0015_*`) cover the 8
//! core tables of the
//! engine DB schema,
//! the three op-log additions (`proposal_ops_log`, `rem_ops_log`,
//! `token_blacklist`), the alignment column (`0011_revoked_by`), the
//! three identity tables (`user_credentials`,
//! `user_invitations`, `consumer_delegations`), and the single-admin
//! partial unique index. The two enrollment tables share migration
//! `0006`.

use std::path::{Path, PathBuf};

use sqlx::{ConnectOptions, SqlitePool, sqlite::SqliteConnectOptions};

use crate::{Error, Result};

/// File name of the engine database inside the workdir.
pub const ENGINE_DB_FILENAME: &str = "engine.db";

/// `sqlx::migrate!()` baked at compile time from `migrations/` at the
/// workspace root. Embedding the SQL into the binary means a fresh
/// checkout (or a release artifact shipped to a user) needs no extra
/// files on disk to bootstrap the schema.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Open (or create) the engine database under `workdir` and apply every
/// pending migration. Returns a ready-to-use connection pool.
///
/// Pragmas applied on connect:
/// - `journal_mode=WAL` — required for single-writer +
///   multi-reader concurrency.
/// - `foreign_keys=ON` — sqlite default is OFF for historical reasons;
///   we always want them enforced.
/// - `busy_timeout=5000` — wait up to 5s for a contending writer before
///   surfacing `SQLITE_BUSY`. The application is single-writer (see
///   [`crate::lockfile`]) so this guards against the brief window where
///   a checkpoint or a concurrent reader holds the lock.
pub async fn open_or_init(workdir: &Path) -> Result<SqlitePool> {
    let db_path = engine_db_path(workdir);

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5))
        .log_statements(tracing::log::LevelFilter::Trace);

    let pool = SqlitePool::connect_with(opts).await?;
    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| Error::Other(format!("migration failed: {e}")))?;
    // Belt-and-suspenders: guarantee the builtin universal `global` group
    // row exists (the migration seeds it; this restores it if it was ever
    // removed out of band). Idempotent and never clobbers an edited scope.
    crate::enrollment::ensure_global_group(&pool)
        .await
        .map_err(|e| Error::Other(format!("seeding global group: {e}")))?;
    Ok(pool)
}

/// Canonical path of the engine database inside a workdir.
#[must_use]
pub fn engine_db_path(workdir: &Path) -> PathBuf {
    workdir.join(ENGINE_DB_FILENAME)
}

/// Read a value from the engine-level key/value table (`engine_meta`,
/// migration 0041). `None` when the key is absent.
///
/// `engine_meta` holds state that is neither per-fact nor per-wiki — today
/// the embedder-identity guard (roadmap 18g).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn meta_get(pool: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM engine_meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// Upsert a value into the engine-level key/value table.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn meta_set(pool: &SqlitePool, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO engine_meta (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Begin a **write** transaction that takes the WAL write lock up front
/// (`BEGIN IMMEDIATE`) instead of sqlx's default `BEGIN DEFERRED`.
///
/// Use this for any multi-statement transaction that writes. A deferred
/// transaction that runs a `SELECT` before its first write pins a read
/// snapshot and must *upgrade* to the write lock at the first
/// `UPDATE`/`INSERT`/`DELETE`; if another connection commits in that gap,
/// `SQLite` rejects the upgrade with `SQLITE_BUSY_SNAPSHOT` — surfaced as
/// `(code: 5) database is locked` — and `busy_timeout` cannot retry a
/// snapshot conflict, so it fails *instantly* rather than waiting. The
/// multi-connection pool makes concurrent writers real (an `events_poll`
/// stamps `last_seen_at` every 30s), so this is not theoretical: it silently
/// broke `events_ack` in v1.5.0. Taking the write lock at `BEGIN` removes the
/// upgrade entirely; `busy_timeout` then serialises contenders normally. See
/// `docs/design-notes/engine-db-and-migrations.md` →
/// "Multi-statement writes must write first".
///
/// Do **not** use this for a transaction that awaits slow non-DB work
/// (network / LLM / filesystem) between begin and commit — it would hold the
/// write lock for that whole span and serialise every other writer. Keep
/// such work outside the transaction instead.
///
/// # Errors
///
/// As [`sqlx::Error`] — including if the connection failed to begin the
/// transaction.
pub async fn begin_immediate(
    pool: &SqlitePool,
) -> sqlx::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: opening a fresh workdir creates the DB, applies every
    /// migration, and the expected canonical tables are present.
    #[tokio::test]
    async fn open_or_init_creates_db_and_applies_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = open_or_init(dir.path()).await.expect("open_or_init");

        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .expect("list tables");

        for expected in [
            "archive_proposals",
            "capture_buffer",
            "consumer_delegations",
            "engine_meta",
            "enrollment_groups",
            "enrollment_users",
            "fact_index",
            "media_catalog",
            "proposal_ops_log",
            "rem_ops_log",
            "structure_proposals",
            "token_blacklist",
            "tool_executions",
            "user_credentials",
            "user_invitations",
            "wiki_events",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing table {expected}; got {names:?}"
            );
        }
    }

    /// Re-opening an existing workdir is idempotent: the migrator
    /// notices it has nothing to apply and returns the same schema.
    #[tokio::test]
    async fn open_or_init_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = open_or_init(dir.path()).await.expect("first open");
        pool.close().await;

        let pool = open_or_init(dir.path()).await.expect("second open");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='fact_index'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn engine_meta_roundtrips_and_upserts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = open_or_init(dir.path()).await.expect("open_or_init");

        assert_eq!(meta_get(&pool, "k").await.unwrap(), None);
        meta_set(&pool, "k", "v1").await.unwrap();
        assert_eq!(meta_get(&pool, "k").await.unwrap().as_deref(), Some("v1"));
        // Upsert overwrites in place.
        meta_set(&pool, "k", "v2").await.unwrap();
        assert_eq!(meta_get(&pool, "k").await.unwrap().as_deref(), Some("v2"));
    }

    /// Migration `0027_op_log_revert_and_briefing_author`
    /// adds three new columns. Verify each one is present with the right
    /// nullability + default after a fresh migration run, and that the
    /// CHECK constraint on `actor_kind` rejects an out-of-enum value.
    /// (cid, name, type, notnull, `dflt_value`, pk) — `PRAGMA table_info`
    /// tuple shape, sqlite-stable across versions.
    type ColRow = (i64, String, String, i64, Option<String>, i64);

    #[tokio::test]
    async fn migration_0027_adds_op_log_and_briefing_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = open_or_init(dir.path()).await.expect("open");

        let oplog_cols: Vec<ColRow> = sqlx::query_as("PRAGMA table_info(wiki_admin_op_log)")
            .fetch_all(&pool)
            .await
            .expect("table_info op_log");
        let actor_kind = oplog_cols
            .iter()
            .find(|c| c.1 == "actor_kind")
            .expect("actor_kind column present");
        assert_eq!(actor_kind.2, "TEXT", "actor_kind is TEXT");
        assert_eq!(actor_kind.3, 1, "actor_kind is NOT NULL");
        assert_eq!(
            actor_kind.4.as_deref(),
            Some("'smart_consumer'"),
            "actor_kind default is 'smart_consumer'",
        );

        let pre_image = oplog_cols
            .iter()
            .find(|c| c.1 == "pre_image_json")
            .expect("pre_image_json column present");
        assert_eq!(pre_image.2, "TEXT", "pre_image_json is TEXT");
        assert_eq!(pre_image.3, 0, "pre_image_json is nullable");

        let briefing_cols: Vec<ColRow> = sqlx::query_as("PRAGMA table_info(wiki_briefing_items)")
            .fetch_all(&pool)
            .await
            .expect("table_info briefing_items");
        let author = briefing_cols
            .iter()
            .find(|c| c.1 == "author_sender_id")
            .expect("author_sender_id column present");
        assert_eq!(author.2, "TEXT", "author_sender_id is TEXT");
        assert_eq!(author.3, 0, "author_sender_id is nullable");

        // CHECK constraint guards `actor_kind` — an out-of-enum value
        // must surface a SQLite constraint violation, not silently land.
        let bogus = sqlx::query(
            "INSERT INTO wiki_admin_op_log
                (wiki_id, sender_id, op_kind, payload_hash, ts, actor_kind)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("test-wiki")
        .bind("alice")
        .bind("push_create")
        .bind("dead")
        .bind("2026-05-26T00:00:00Z")
        .bind("bogus_actor")
        .execute(&pool)
        .await;
        assert!(
            bogus.is_err(),
            "CHECK on actor_kind must reject out-of-enum values; got {bogus:?}",
        );

        // The three allowed values land cleanly.
        for kind in ["smart_consumer", "dashboard", "system"] {
            sqlx::query(
                "INSERT INTO wiki_admin_op_log
                    (wiki_id, sender_id, op_kind, payload_hash, ts, actor_kind)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind("test-wiki")
            .bind("alice")
            .bind("push_create")
            .bind("dead")
            .bind("2026-05-26T00:00:00Z")
            .bind(kind)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("inserting actor_kind={kind} must succeed: {e}"));
        }
    }

    /// WAL journal mode survives the migration step — Pragma persists
    /// on the file once applied (sqlite stores it in the header).
    #[tokio::test]
    async fn wal_journal_mode_is_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = open_or_init(dir.path()).await.expect("open");

        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("pragma");
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }
}
