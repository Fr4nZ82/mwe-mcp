// SPDX-License-Identifier: AGPL-3.0-or-later
//! `wiki_sections` + `smart_wikis` — the smart-wiki content index.
//!
//! A **smart wiki** is a project wiki a smart consumer authors verbatim
//! through the family-H `wiki_admin_*` tools ([smart
//! wikis](../../../docs/design-notes/smart-wikis.md)). Its pages carry no
//! per-fragment `{{f=…}}` markers, so recall indexes their **content**:
//! each page is chunked into heading-delimited sections, each section is
//! embedded, and each becomes one row here.
//!
//! These rows are deliberately **not** [`crate::fact_index`] rows. A fact
//! is an authoritative, per-fragment-governed claim with a lifecycle —
//! supersedence chains, tombstones, validity windows, cross-user
//! attribution. A section is none of that:
//!
//! | | fact ([`crate::fact_index`]) | section (here) |
//! |---|---|---|
//! | origin | captured from a conversation | derived from a file on disk |
//! | if lost | gone (back it up) | re-derived by the next reindex |
//! | read access | per fragment (`owner` / `allow` / `sender`) | the **wiki's**, from `smart_wikis` |
//! | lifecycle | supersede / forget / validity | none — it exists while its page does |
//!
//! Keeping both in one table left most fact columns permanently NULL on
//! section rows, made "search everything except the project docs"
//! impossible to express in SQL (the smart flag lives on disk, in each
//! wiki's `_meta.md`), and turned a sharing edit into a rewrite of one
//! row per section instead of one row per wiki.
//!
//! ## Identity is positional
//!
//! A section is keyed by `(source_path, section_ord)` — its position on
//! its page — not by a minted id. Re-indexing an unchanged page is a
//! no-op, and a section that keeps its position keeps its recall history
//! across edits elsewhere on the page. [`SectionRow::handle`] renders the
//! pair as the single string external logs use.
//!
//! ## `smart_wikis` is a projection, not the source of truth
//!
//! Each wiki's `_meta.md` on disk stays authoritative — a hand edit still
//! wins, and the tree-walking sweep re-projects it. This table exists so
//! "which wikis are smart?" and "who may read this wiki?" can be part of
//! a SQL query instead of a per-hit tree walk.

use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqlitePool};
use thiserror::Error;

use crate::fact_index::{decode_embedding, encode_embedding};
use crate::types::Principal;

/// Errors raised by the section-index layer.
#[derive(Debug, Error)]
pub enum SectionError {
    /// Underlying `SQLite` error.
    #[error("wiki_sections db: {0}")]
    Db(#[from] sqlx::Error),

    /// JSON serialization failure on `shared_with`.
    #[error("wiki_sections json: {0}")]
    Json(#[from] serde_json::Error),

    /// A stored principal string did not parse.
    #[error("wiki_sections principal: {0}")]
    Principal(String),

    /// The embedding column was stored as a length that is not a
    /// multiple of 4 bytes (i.e. cannot be decoded as `f32` little-endian).
    #[error("wiki_sections embedding blob length {0} is not divisible by 4")]
    InvalidEmbeddingBlob(usize),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, SectionError>;

// ---------- SectionRow ----------

/// One row of `wiki_sections`, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionRow {
    /// Workdir-relative path of the page this section belongs to.
    pub source_path: String,
    /// 0-based position of the section on its page.
    pub section_ord: i64,
    /// `wiki_id` of the containing smart wiki.
    pub wiki_id: String,
    /// Heading chain (`"A > B > C"`), `None` for a page preamble.
    pub heading_path: Option<String>,
    /// Exactly what was embedded: the heading chain plus the body.
    pub text: String,
    /// Decoded embedding vector.
    pub embedding: Vec<f32>,
    /// Vector width, stored explicitly for embedder migrations.
    pub embedding_dim: i64,
    /// First time this position was indexed.
    pub created_at: String,
    /// Last time this position's content changed.
    pub updated_at: String,
    /// Last time this section surfaced in a recall top-K.
    pub last_recall_at: Option<String>,
    /// Rolling 30-day recall counter — REM's recall-hot signal.
    pub recall_count_30d: i64,
}

impl SectionRow {
    /// The single-string handle external logs reference a section by:
    /// `"<source_path>#<section_ord>"`.
    ///
    /// Deterministic, so a handle logged today still resolves after the
    /// page is re-indexed — unlike the minted per-reindex ids this table
    /// replaced.
    #[must_use]
    pub fn handle(&self) -> String {
        format!("{}#{}", self.source_path, self.section_ord)
    }
}

/// A section about to be written. `created_at` / recall telemetry are
/// owned by the store, not the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSection {
    /// `wiki_id` of the containing smart wiki.
    pub wiki_id: String,
    /// Workdir-relative path of the page.
    pub source_path: String,
    /// 0-based position on the page.
    pub section_ord: i64,
    /// Heading chain, `None` for a page preamble.
    pub heading_path: Option<String>,
    /// Heading chain plus body — the embedded text.
    pub text: String,
    /// Pre-computed embedding. Callers embed **before** opening the
    /// write transaction; see [`replace_page_sections`].
    pub embedding: Vec<f32>,
}

#[derive(sqlx::FromRow)]
struct RawSectionRow {
    source_path: String,
    section_ord: i64,
    wiki_id: String,
    heading_path: Option<String>,
    text: String,
    embedding: Vec<u8>,
    embedding_dim: i64,
    created_at: String,
    updated_at: String,
    last_recall_at: Option<String>,
    recall_count_30d: i64,
}

fn decode_section(raw: RawSectionRow) -> Result<SectionRow> {
    let embedding = decode_embedding(&raw.embedding)
        .map_err(|_| SectionError::InvalidEmbeddingBlob(raw.embedding.len()))?;
    Ok(SectionRow {
        source_path: raw.source_path,
        section_ord: raw.section_ord,
        wiki_id: raw.wiki_id,
        heading_path: raw.heading_path,
        text: raw.text,
        embedding,
        embedding_dim: raw.embedding_dim,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        last_recall_at: raw.last_recall_at,
        recall_count_30d: raw.recall_count_30d,
    })
}

const SECTION_COLUMNS: &str = r#"source_path, section_ord, wiki_id, heading_path, "text",
                                 embedding, embedding_dim, created_at, updated_at,
                                 last_recall_at, recall_count_30d"#;

// ---------- Write path ----------

/// Replace a page's sections with `sections`, atomically.
///
/// Upserts by position and then drops any tail position the new content
/// no longer reaches, all in **one transaction**. Doing it atomically is
/// what makes concurrent reindexers of the same page (the push-enqueued
/// index, the filesystem watcher, the safety-net sweep) converge to one
/// clean set: `SQLite` serializes writers, so a second pass observes the
/// first's committed rows instead of interleaving.
///
/// A position whose text is unchanged keeps its `created_at` **and** its
/// recall telemetry; a position whose text changed keeps `created_at`
/// (the slot is the same) but resets the counters, because the recall
/// history belonged to the old content.
///
/// Embeddings must already be computed and carried in `sections` — the
/// transaction does only fast DB work. Holding a write transaction
/// across a network embed would block every other writer.
///
/// Returns `(upserted, dropped_tail)`.
///
/// # Errors
///
/// `sqlx::Error`; the transaction is rolled back on any error.
pub async fn replace_page_sections(
    pool: &SqlitePool,
    source_path: &str,
    sections: &[NewSection],
) -> Result<(u64, u64)> {
    let now = chrono::Utc::now().to_rfc3339();
    // Write-first: the first statement of the transaction is an INSERT,
    // so a plain `begin` cannot hit SQLITE_BUSY_SNAPSHOT the way a
    // read-then-write transaction can.
    let mut tx = pool.begin().await?;
    let mut upserted = 0_u64;
    for section in sections {
        upserted += upsert_section(&mut *tx, section, &now).await?;
    }
    let tail = i64::try_from(sections.len()).unwrap_or(i64::MAX);
    let dropped =
        sqlx::query("DELETE FROM wiki_sections WHERE source_path = ? AND section_ord >= ?")
            .bind(source_path)
            .bind(tail)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    tx.commit().await?;
    Ok((upserted, dropped))
}

/// Upsert one section on any executor — a pool *or* an open transaction.
async fn upsert_section<'e, E>(executor: E, section: &NewSection, now: &str) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let blob = encode_embedding(&section.embedding);
    let dim = i64::try_from(section.embedding.len()).unwrap_or(i64::MAX);
    // The CASE arms read the pre-update row, so they compare the stored
    // text against the incoming one: same text keeps the recall history,
    // changed text resets it.
    let res = sqlx::query(
        r#"INSERT INTO wiki_sections
               (source_path, section_ord, wiki_id, heading_path, "text",
                embedding, embedding_dim, created_at, updated_at,
                last_recall_at, recall_count_30d)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 0)
           ON CONFLICT(source_path, section_ord) DO UPDATE SET
               wiki_id          = excluded.wiki_id,
               heading_path     = excluded.heading_path,
               embedding        = excluded.embedding,
               embedding_dim    = excluded.embedding_dim,
               updated_at       = excluded.updated_at,
               last_recall_at   = CASE WHEN wiki_sections."text" = excluded."text"
                                       THEN wiki_sections.last_recall_at ELSE NULL END,
               recall_count_30d = CASE WHEN wiki_sections."text" = excluded."text"
                                       THEN wiki_sections.recall_count_30d ELSE 0 END,
               "text"           = excluded."text""#,
    )
    .bind(&section.source_path)
    .bind(section.section_ord)
    .bind(&section.wiki_id)
    .bind(section.heading_path.as_deref())
    .bind(&section.text)
    .bind(blob)
    .bind(dim)
    .bind(now)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(res.rows_affected())
}

/// Drop every section of one page. Returns the row count.
///
/// The markerless counterpart of a standard wiki's orphan tombstone: a
/// removed smart page's sections simply disappear, because the page is
/// their only source.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn drop_page_sections(pool: &SqlitePool, source_path: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM wiki_sections WHERE source_path = ?")
        .bind(source_path)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Drop every section of one wiki. Returns the row count.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn drop_wiki_sections(pool: &SqlitePool, wiki_id: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM wiki_sections WHERE wiki_id = ?")
        .bind(wiki_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ---------- Read path ----------

/// Every section of one page, ordered by position.
///
/// # Errors
///
/// `sqlx::Error` + embedding decode failures.
pub async fn find_page_sections(pool: &SqlitePool, source_path: &str) -> Result<Vec<SectionRow>> {
    let sql = format!(
        "SELECT {SECTION_COLUMNS} FROM wiki_sections WHERE source_path = ? ORDER BY section_ord ASC"
    );
    let raw = sqlx::query_as::<_, RawSectionRow>(&sql)
        .bind(source_path)
        .fetch_all(pool)
        .await?;
    raw.into_iter().map(decode_section).collect()
}

/// Every section of one wiki, ordered by page then position.
///
/// # Errors
///
/// `sqlx::Error` + embedding decode failures.
pub async fn find_wiki_sections(pool: &SqlitePool, wiki_id: &str) -> Result<Vec<SectionRow>> {
    let sql = format!(
        "SELECT {SECTION_COLUMNS} FROM wiki_sections WHERE wiki_id = ? \
         ORDER BY source_path ASC, section_ord ASC"
    );
    let raw = sqlx::query_as::<_, RawSectionRow>(&sql)
        .bind(wiki_id)
        .fetch_all(pool)
        .await?;
    raw.into_iter().map(decode_section).collect()
}

/// Recall candidates: every section of the wikis in `wiki_ids`.
///
/// The caller resolves read access **once per wiki** (from
/// [`list_smart_wikis`]) and passes only the readable ids, so the bytes
/// of an unreadable wiki are never read off disk — unlike a per-row ACL
/// filter, which must load everything before it can discard anything.
///
/// An empty `wiki_ids` returns no rows without touching the DB.
///
/// # Errors
///
/// `sqlx::Error` + embedding decode failures.
pub async fn find_candidates_in_wikis(
    pool: &SqlitePool,
    wiki_ids: &[String],
) -> Result<Vec<SectionRow>> {
    if wiki_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", wiki_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT {SECTION_COLUMNS} FROM wiki_sections WHERE wiki_id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, RawSectionRow>(&sql);
    for id in wiki_ids {
        q = q.bind(id);
    }
    let raw = q.fetch_all(pool).await?;
    raw.into_iter().map(decode_section).collect()
}

/// Distinct `(wiki_id, source_path)` pairs currently indexed — the input
/// of the deleted-page sweep, which drops the sections of any page that
/// no longer exists on disk.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn indexed_pages(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT DISTINCT wiki_id, source_path FROM wiki_sections")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

/// Bump the recall telemetry of the given `(source_path, section_ord)`
/// positions — the section counterpart of
/// [`crate::fact_index::bump_recall_hits`].
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn bump_recall_hits(pool: &SqlitePool, hits: &[(String, i64)]) -> Result<u64> {
    if hits.is_empty() {
        return Ok(0);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    let mut affected = 0_u64;
    for (source_path, ord) in hits {
        affected += sqlx::query(
            "UPDATE wiki_sections SET last_recall_at = ?, recall_count_30d = recall_count_30d + 1 \
             WHERE source_path = ? AND section_ord = ?",
        )
        .bind(&now)
        .bind(source_path)
        .bind(ord)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

// ---------- smart_wikis: the queryable projection of `_meta.md` ----------

/// One row of `smart_wikis`, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartWikiRow {
    /// The wiki's id.
    pub wiki_id: String,
    /// Resolved owner principal (the wiki's `scope`).
    pub owner_id: Principal,
    /// The `_meta.shared_with` roster.
    pub shared_with: Vec<Principal>,
    /// `_meta.extra.project_id`, when the wiki carries one.
    pub project_id: Option<String>,
    /// Free-form tone/label. Does **not** decide smart-ness.
    pub wiki_type: String,
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct PrincipalWire(String);

fn principals_to_json(ps: &[Principal]) -> std::result::Result<String, serde_json::Error> {
    let wire: Vec<PrincipalWire> = ps.iter().map(|p| PrincipalWire(p.to_string())).collect();
    serde_json::to_string(&wire)
}

fn principals_from_json(s: &str) -> Result<Vec<Principal>> {
    let wire: Vec<PrincipalWire> = serde_json::from_str(s)
        .map_err(|e| SectionError::Principal(format!("shared_with: {e}")))?;
    wire.into_iter()
        .map(|w| {
            w.0.parse::<Principal>()
                .map_err(|e| SectionError::Principal(e.to_string()))
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct RawSmartWikiRow {
    wiki_id: String,
    owner_id: String,
    shared_with: String,
    project_id: Option<String>,
    wiki_type: String,
}

fn decode_smart_wiki(raw: RawSmartWikiRow) -> Result<SmartWikiRow> {
    Ok(SmartWikiRow {
        wiki_id: raw.wiki_id,
        owner_id: raw
            .owner_id
            .parse::<Principal>()
            .map_err(|e| SectionError::Principal(e.to_string()))?,
        shared_with: principals_from_json(&raw.shared_with)?,
        project_id: raw.project_id,
        wiki_type: raw.wiki_type,
    })
}

/// Project one smart wiki's `_meta.md` into the registry.
///
/// Idempotent: re-projecting an unchanged wiki rewrites the same values.
/// This is the **only** write path for wiki-level smart-wiki ACL — a
/// sharing edit touches this one row, where it used to rewrite one row
/// per indexed section.
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `shared_with`.
pub async fn upsert_smart_wiki(pool: &SqlitePool, wiki: &SmartWikiRow) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let shared = principals_to_json(&wiki.shared_with)?;
    sqlx::query(
        "INSERT INTO smart_wikis
             (wiki_id, owner_id, shared_with, project_id, wiki_type, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(wiki_id) DO UPDATE SET
             owner_id    = excluded.owner_id,
             shared_with = excluded.shared_with,
             project_id  = excluded.project_id,
             wiki_type   = excluded.wiki_type,
             updated_at  = excluded.updated_at",
    )
    .bind(&wiki.wiki_id)
    .bind(wiki.owner_id.to_string())
    .bind(&shared)
    .bind(wiki.project_id.as_deref())
    .bind(&wiki.wiki_type)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop a wiki from the registry (it stopped being smart, or it is gone).
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn remove_smart_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM smart_wikis WHERE wiki_id = ?")
        .bind(wiki_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// The whole registry — a handful of rows, cheap to load per query.
///
/// # Errors
///
/// `sqlx::Error` + principal decode failures.
pub async fn list_smart_wikis(pool: &SqlitePool) -> Result<Vec<SmartWikiRow>> {
    let raw = sqlx::query_as::<_, RawSmartWikiRow>(
        "SELECT wiki_id, owner_id, shared_with, project_id, wiki_type FROM smart_wikis",
    )
    .fetch_all(pool)
    .await?;
    raw.into_iter().map(decode_smart_wiki).collect()
}

/// One registry row, when the wiki is smart.
///
/// # Errors
///
/// `sqlx::Error` + principal decode failures.
pub async fn find_smart_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<Option<SmartWikiRow>> {
    let raw = sqlx::query_as::<_, RawSmartWikiRow>(
        "SELECT wiki_id, owner_id, shared_with, project_id, wiki_type FROM smart_wikis \
         WHERE wiki_id = ?",
    )
    .bind(wiki_id)
    .fetch_optional(pool)
    .await?;
    raw.map(decode_smart_wiki).transpose()
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

    fn section(path: &str, ord: i64, text: &str) -> NewSection {
        NewSection {
            wiki_id: "alice-proj".to_owned(),
            source_path: path.to_owned(),
            section_ord: ord,
            heading_path: Some(format!("H{ord}")),
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3],
        }
    }

    #[tokio::test]
    async fn replace_page_sections_inserts_and_reads_back_in_order() {
        let pool = pool().await;
        let page = "wikis/alice/proj/index.md";
        let (upserted, dropped) = replace_page_sections(
            &pool,
            page,
            &[section(page, 0, "intro"), section(page, 1, "body")],
        )
        .await
        .unwrap();
        assert_eq!((upserted, dropped), (2, 0));

        let rows = find_page_sections(&pool, page).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].section_ord, 0);
        assert_eq!(rows[0].text, "intro");
        assert_eq!(rows[1].text, "body");
        assert_eq!(rows[0].embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(rows[0].embedding_dim, 3);
        assert_eq!(rows[0].handle(), format!("{page}#0"));
    }

    #[tokio::test]
    async fn unchanged_section_keeps_created_at_and_recall_history() {
        let pool = pool().await;
        let page = "wikis/alice/proj/index.md";
        replace_page_sections(
            &pool,
            page,
            &[section(page, 0, "stable"), section(page, 1, "old")],
        )
        .await
        .unwrap();
        bump_recall_hits(&pool, &[(page.to_owned(), 0), (page.to_owned(), 1)])
            .await
            .unwrap();
        let before = find_page_sections(&pool, page).await.unwrap();
        assert_eq!(before[0].recall_count_30d, 1);
        assert_eq!(before[1].recall_count_30d, 1);

        // Position 0 unchanged, position 1 rewritten.
        replace_page_sections(
            &pool,
            page,
            &[section(page, 0, "stable"), section(page, 1, "new")],
        )
        .await
        .unwrap();
        let after = find_page_sections(&pool, page).await.unwrap();
        assert_eq!(after[0].created_at, before[0].created_at);
        assert_eq!(
            after[0].recall_count_30d, 1,
            "unchanged text keeps its recall history"
        );
        assert!(after[0].last_recall_at.is_some());
        assert_eq!(
            after[1].recall_count_30d, 0,
            "changed text resets the recall history of that slot"
        );
        assert!(after[1].last_recall_at.is_none());
        assert_eq!(
            after[1].created_at, before[1].created_at,
            "the slot itself is the same, only its content changed"
        );
    }

    #[tokio::test]
    async fn shrinking_a_page_drops_the_tail_positions() {
        let pool = pool().await;
        let page = "wikis/alice/proj/index.md";
        replace_page_sections(
            &pool,
            page,
            &[
                section(page, 0, "a"),
                section(page, 1, "b"),
                section(page, 2, "c"),
            ],
        )
        .await
        .unwrap();
        let (_, dropped) = replace_page_sections(&pool, page, &[section(page, 0, "a")])
            .await
            .unwrap();
        assert_eq!(dropped, 2);
        let rows = find_page_sections(&pool, page).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn replace_is_idempotent_on_an_unchanged_page() {
        let pool = pool().await;
        let page = "wikis/alice/proj/index.md";
        let desired = [section(page, 0, "a"), section(page, 1, "b")];
        replace_page_sections(&pool, page, &desired).await.unwrap();
        let first = find_page_sections(&pool, page).await.unwrap();
        replace_page_sections(&pool, page, &desired).await.unwrap();
        let second = find_page_sections(&pool, page).await.unwrap();
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.text, b.text);
            assert_eq!(a.created_at, b.created_at);
            assert_eq!(a.recall_count_30d, b.recall_count_30d);
        }
    }

    #[tokio::test]
    async fn candidates_are_scoped_to_the_readable_wikis() {
        let pool = pool().await;
        let mine = "wikis/alice/proj/index.md";
        let theirs = "wikis/bob/proj/index.md";
        replace_page_sections(&pool, mine, &[section(mine, 0, "mine")])
            .await
            .unwrap();
        let mut other = section(theirs, 0, "theirs");
        other.wiki_id = "bob-proj".to_owned();
        replace_page_sections(&pool, theirs, &[other])
            .await
            .unwrap();

        let visible = find_candidates_in_wikis(&pool, &["alice-proj".to_owned()])
            .await
            .unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].text, "mine");

        let none = find_candidates_in_wikis(&pool, &[]).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn dropping_a_page_and_a_wiki_removes_their_sections() {
        let pool = pool().await;
        let one = "wikis/alice/proj/one.md";
        let two = "wikis/alice/proj/two.md";
        replace_page_sections(&pool, one, &[section(one, 0, "a")])
            .await
            .unwrap();
        replace_page_sections(&pool, two, &[section(two, 0, "b")])
            .await
            .unwrap();
        assert_eq!(indexed_pages(&pool).await.unwrap().len(), 2);

        assert_eq!(drop_page_sections(&pool, one).await.unwrap(), 1);
        assert_eq!(indexed_pages(&pool).await.unwrap().len(), 1);
        assert_eq!(drop_wiki_sections(&pool, "alice-proj").await.unwrap(), 1);
        assert!(indexed_pages(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn smart_wiki_registry_upserts_and_reads_back() {
        let pool = pool().await;
        let row = SmartWikiRow {
            wiki_id: "alice-proj".to_owned(),
            owner_id: Principal::User("alice".to_owned()),
            shared_with: vec![
                Principal::User("bob".to_owned()),
                Principal::Group("devs".to_owned()),
            ],
            project_id: Some("abc123".to_owned()),
            wiki_type: "project".to_owned(),
        };
        upsert_smart_wiki(&pool, &row).await.unwrap();
        let read = find_smart_wiki(&pool, "alice-proj").await.unwrap().unwrap();
        assert_eq!(read, row);

        // A sharing edit is one row, not one row per section.
        let revoked = SmartWikiRow {
            shared_with: Vec::new(),
            ..row
        };
        upsert_smart_wiki(&pool, &revoked).await.unwrap();
        let read = find_smart_wiki(&pool, "alice-proj").await.unwrap().unwrap();
        assert!(read.shared_with.is_empty());
        assert_eq!(list_smart_wikis(&pool).await.unwrap().len(), 1);

        assert_eq!(remove_smart_wiki(&pool, "alice-proj").await.unwrap(), 1);
        assert!(
            find_smart_wiki(&pool, "alice-proj")
                .await
                .unwrap()
                .is_none()
        );
    }
}
