// SPDX-License-Identifier: AGPL-3.0-or-later
//! `page_card` — a page's one-line card, in a table you can query.
//!
//! A page's **card** is its testata `description`: the single line saying
//! what belongs on that page. It is what the recall navigator is shown when
//! it decides whether to open the page, and for a page no `[[wikilink]]`
//! points at, it is the only thing that can bring a reader there. It is
//! authored on the write side — by the Cartografo when it proposes a page,
//! by the compiler when it renders one, by the operator editing a testata —
//! and until this table existed it lived in exactly one place, the `.md`
//! frontmatter, so the only way to ask anything *about* the cards was to open
//! every page and parse its YAML.
//!
//! ## The file is the truth; this is a cache
//!
//! Same class as the smart-wiki projection of `_meta.md` in
//! [`crate::sections`]: a hand edit in Obsidian wins, and so does the nightly
//! compile that rewrites pages wholesale. Three properties keep that honest:
//!
//! - rows are written by the reindex pipeline ([`crate::reindex`]), the one
//!   path every page change already flows through — the watcher on each edit,
//!   `reindex_full` on cold start and as the missed-event net;
//! - a **missing** row is never an error: every reader falls back to opening
//!   the page, so an empty table degrades to the behaviour that shipped
//!   before it, and `rm engine.db` + a reindex rebuilds it;
//! - a **stale** row is caught before it is shown, by the stamp.
//!
//! ## The stamp
//!
//! [`file_stamp`] is `(mtime_ms, size)`. The display path compares it against
//! the file it is about to describe and falls back to reading when they
//! disagree — a `stat` in place of a read plus a YAML parse. It is
//! deliberately not a hash: hashing means reading, which is the cost being
//! avoided. What it cannot catch is an edit that changes neither size nor
//! mtime millisecond; the cost of that is one stale line for the seconds
//! until the watcher rewrites the row.
//!
//! The **ranking** side (selecting which cards to show by similarity) does
//! not check the stamp at all: a slightly stale description changes which
//! pages are *offered*, never what is *shown*, and an offer is approximate by
//! nature.

use std::path::Path;

use sqlx::{Row as _, SqlitePool};
use thiserror::Error;

use crate::fact_index::{decode_embedding, encode_embedding};

/// Errors raised by the page-card layer.
#[derive(Debug, Error)]
pub enum PageCardError {
    /// Underlying `SQLite` error.
    #[error("page_card db: {0}")]
    Db(#[from] sqlx::Error),

    /// JSON serialization failure on `keywords`.
    #[error("page_card json: {0}")]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, PageCardError>;

/// One page's card as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct PageCardRow {
    /// Workdir-relative page path — the key.
    pub source_path: String,
    /// The wiki the page lives in.
    pub wiki_id: String,
    /// The card itself. `None` when the page's testata carries none.
    pub description: Option<String>,
    /// Flattened testata `keywords`, **owner-tier** — never the
    /// reader-relative topic union recall matches on
    /// (`meta_annotate::build_reader_card`).
    pub keywords: Vec<String>,
    /// Writing style from the testata — one of the three, or nothing.
    pub style: Option<crate::wiki::PageStyle>,
    /// Validity stamp of the file this row was read from.
    pub file_mtime_ms: Option<i64>,
    /// Second half of the stamp.
    pub file_size: Option<i64>,
    /// The card's vector — `None` until the card selection embeds it.
    pub embedding: Option<Vec<f32>>,
}

impl PageCardRow {
    /// Whether this row still describes the file at `abs_path`.
    ///
    /// `false` for an unstamped row and for a file that cannot be stat'ed:
    /// both mean "cannot vouch for it", and the caller's fallback is to open
    /// the page, which is never wrong.
    #[must_use]
    pub fn matches_file(&self, abs_path: &Path) -> bool {
        let (Some(mtime), Some(size)) = (self.file_mtime_ms, self.file_size) else {
            return false;
        };
        file_stamp(abs_path).is_some_and(|(m, s)| m == mtime && s == size)
    }
}

/// One card to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPageCard {
    /// Workdir-relative page path.
    pub source_path: String,
    /// The wiki the page lives in.
    pub wiki_id: String,
    /// The card, when the testata has one.
    pub description: Option<String>,
    /// Owner-tier testata keywords.
    pub keywords: Vec<String>,
    /// Writing style — one of the three, or nothing.
    pub style: Option<crate::wiki::PageStyle>,
    /// Stamp of the file it was read from, when it could be stat'ed.
    pub file_mtime_ms: Option<i64>,
    /// Second half of the stamp.
    pub file_size: Option<i64>,
}

/// `(mtime_ms, size)` of a file, or `None` when it cannot be stat'ed.
///
/// Milliseconds, not nanoseconds: the value round-trips through `SQLite` as an
/// `INTEGER`, and a millisecond is finer than any editor's write cadence
/// while staying far inside `i64` for dates a filesystem can hold.
#[must_use]
pub fn file_stamp(abs_path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(abs_path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let ms = i64::try_from(mtime.as_millis()).ok()?;
    let size = i64::try_from(meta.len()).ok()?;
    Some((ms, size))
}

// ---------- Write path ----------

/// Store one page's card, replacing whatever was there.
///
/// **The embedding survives only an unchanged description.** It encodes that
/// exact sentence, so a rewritten card with a stale vector would be ranked by
/// what the page used to be for — worse than not being ranked at all. The
/// `CASE` arms read the pre-update row, so the comparison is against what was
/// stored.
///
/// # Errors
///
/// `sqlx::Error`, or a `keywords` that will not serialise.
pub async fn upsert(pool: &SqlitePool, card: &NewPageCard) -> Result<u64> {
    let keywords = serde_json::to_string(&card.keywords)?;
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        r"INSERT INTO page_card
              (source_path, wiki_id, description, keywords, style,
               file_mtime_ms, file_size, embedding, embedding_dim, updated_at)
          VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?)
          ON CONFLICT(source_path) DO UPDATE SET
              wiki_id       = excluded.wiki_id,
              keywords      = excluded.keywords,
              style         = excluded.style,
              file_mtime_ms = excluded.file_mtime_ms,
              file_size     = excluded.file_size,
              updated_at    = excluded.updated_at,
              embedding     = CASE WHEN page_card.description IS excluded.description
                                   THEN page_card.embedding ELSE NULL END,
              embedding_dim = CASE WHEN page_card.description IS excluded.description
                                   THEN page_card.embedding_dim ELSE NULL END,
              description   = excluded.description",
    )
    .bind(&card.source_path)
    .bind(&card.wiki_id)
    .bind(card.description.as_deref())
    .bind(&keywords)
    .bind(card.style.map(crate::wiki::PageStyle::as_str))
    .bind(card.file_mtime_ms)
    .bind(card.file_size)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Attach a card's vector. No-op on a row that does not exist.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn set_embedding(pool: &SqlitePool, source_path: &str, embedding: &[f32]) -> Result<u64> {
    let dim = i64::try_from(embedding.len()).unwrap_or(i64::MAX);
    let res =
        sqlx::query("UPDATE page_card SET embedding = ?, embedding_dim = ? WHERE source_path = ?")
            .bind(encode_embedding(embedding))
            .bind(dim)
            .bind(source_path)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Drop one page's card — the page is gone.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn drop_page(pool: &SqlitePool, source_path: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM page_card WHERE source_path = ?")
        .bind(source_path)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Drop every card of one wiki.
///
/// Called where the per-page sweep cannot reach: the admin delete
/// ([`crate::wiki_delete::delete_wiki_subtree`]) and the moment a wiki turns
/// smart ([`crate::reindex`]). Both leave the wiki outside the sweep's walk —
/// it only visits wikis still discovered as standard — so without this the
/// rows, embeddings included, sit in the table forever.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn drop_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM page_card WHERE wiki_id = ?")
        .bind(wiki_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ---------- Read path ----------

fn row_of(row: &sqlx::sqlite::SqliteRow) -> PageCardRow {
    let keywords: String = row.get("keywords");
    let blob: Option<Vec<u8>> = row.get("embedding");
    PageCardRow {
        source_path: row.get("source_path"),
        wiki_id: row.get("wiki_id"),
        description: row.get("description"),
        keywords: serde_json::from_str(&keywords).unwrap_or_default(),
        style: crate::wiki::PageStyle::parse_lenient(
            row.get::<Option<String>, _>("style").as_deref(),
        ),
        file_mtime_ms: row.get("file_mtime_ms"),
        file_size: row.get("file_size"),
        // A blob that will not decode is a vector from another embedder or a
        // truncated write: it reads as "no vector", which drops the row out
        // of the ranking rather than poisoning it.
        embedding: blob.as_deref().and_then(|b| decode_embedding(b).ok()),
    }
}

const SELECT_COLS: &str = "source_path, wiki_id, description, keywords, style, \
                           file_mtime_ms, file_size, embedding";

/// One page's card, or `None` when the table has never seen it.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn get(pool: &SqlitePool, source_path: &str) -> Result<Option<PageCardRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM page_card WHERE source_path = ?");
    let row = sqlx::query(&sql)
        .bind(source_path)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_of))
}

/// Every card of one wiki, ordered by page path.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn list_for_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<Vec<PageCardRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM page_card WHERE wiki_id = ? ORDER BY source_path");
    let rows = sqlx::query(&sql).bind(wiki_id).fetch_all(pool).await?;
    Ok(rows.iter().map(row_of).collect())
}

/// Every card in the memory, ordered by page path.
///
/// The card selection's candidate set. Deliberately unfiltered: which cards a
/// reader may *see* is decided per reader, downstream.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn list_all(pool: &SqlitePool) -> Result<Vec<PageCardRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM page_card ORDER BY source_path");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows.iter().map(row_of).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool **and the workdir it lives in**. Dropping the `TestWorkdir`
    /// deletes the directory holding the `SQLite` file, so the handle has to
    /// outlive the pool — discarding it passes when run alone and fails
    /// under the parallel suite, which is the worst way to find out.
    async fn pool() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    fn card(path: &str, desc: Option<&str>) -> NewPageCard {
        NewPageCard {
            source_path: path.to_owned(),
            wiki_id: "alice".to_owned(),
            description: desc.map(str::to_owned),
            keywords: vec!["cucina".to_owned()],
            style: Some(crate::wiki::PageStyle::Prosa),
            file_mtime_ms: Some(1_000),
            file_size: Some(42),
        }
    }

    #[tokio::test]
    async fn a_card_round_trips_and_an_absent_one_is_not_an_error() {
        let (_workdir, pool) = pool().await;
        assert!(
            get(&pool, "wikis/alice/cucina.md")
                .await
                .expect("get")
                .is_none(),
            "a page the table has never seen reads as absent, never as a failure"
        );
        upsert(
            &pool,
            &card("wikis/alice/cucina.md", Some("what gets cooked")),
        )
        .await
        .expect("upsert");
        let row = get(&pool, "wikis/alice/cucina.md")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.description.as_deref(), Some("what gets cooked"));
        assert_eq!(row.keywords, vec!["cucina".to_owned()]);
        assert_eq!(row.style, Some(crate::wiki::PageStyle::Prosa));
    }

    /// The vector encodes one sentence. Keeping it across a rewritten card
    /// would rank the page by what it used to be for.
    #[tokio::test]
    async fn a_rewritten_card_drops_its_vector_and_an_unchanged_one_keeps_it() {
        let (_workdir, pool) = pool().await;
        let path = "wikis/alice/cucina.md";
        upsert(&pool, &card(path, Some("what gets cooked")))
            .await
            .expect("upsert");
        set_embedding(&pool, path, &[0.1, 0.2, 0.3])
            .await
            .expect("embed");

        // Same description, different stamp: the vector still describes it.
        let mut again = card(path, Some("what gets cooked"));
        again.file_size = Some(99);
        upsert(&pool, &again).await.expect("re-upsert");
        assert!(
            get(&pool, path)
                .await
                .expect("get")
                .expect("row")
                .embedding
                .is_some(),
            "an unchanged card keeps the vector it paid for"
        );

        upsert(&pool, &card(path, Some("the weekly menu")))
            .await
            .expect("rewrite");
        assert!(
            get(&pool, path)
                .await
                .expect("get")
                .expect("row")
                .embedding
                .is_none(),
            "a rewritten card cannot keep a vector of the old sentence"
        );
    }

    /// An unstamped row can never vouch for a file: the fallback is to open
    /// the page, which is never wrong.
    #[tokio::test]
    async fn an_unstamped_row_never_matches_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("cucina.md");
        std::fs::write(&page, "---\ndescription: \"d\"\n---\n\nbody\n").unwrap();
        let (_workdir, pool) = pool().await;
        let mut c = card("wikis/alice/cucina.md", Some("d"));
        c.file_mtime_ms = None;
        c.file_size = None;
        upsert(&pool, &c).await.expect("upsert");
        let row = get(&pool, "wikis/alice/cucina.md")
            .await
            .expect("get")
            .expect("row");
        assert!(!row.matches_file(&page));

        // Stamped from the real file: it vouches — until the file changes.
        let (m, s) = file_stamp(&page).expect("stamp");
        let mut c = card("wikis/alice/cucina.md", Some("d"));
        c.file_mtime_ms = Some(m);
        c.file_size = Some(s);
        upsert(&pool, &c).await.expect("upsert");
        let row = get(&pool, "wikis/alice/cucina.md")
            .await
            .expect("get")
            .expect("row");
        assert!(
            row.matches_file(&page),
            "a fresh stamp vouches for the file"
        );
        std::fs::write(
            &page,
            "---\ndescription: \"a much longer line now\"\n---\n\nbody\n",
        )
        .unwrap();
        assert!(
            !row.matches_file(&page),
            "an edited file stops matching, so the reader falls back to opening it"
        );
    }
}
