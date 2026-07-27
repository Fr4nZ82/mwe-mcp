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

// ---------- Lexical (exact-term) search ----------

/// How many query words reach the index. A turn is a sentence, not a
/// document; past this the tail is noise that only widens the `OR`.
const LEXICAL_MAX_TERMS: usize = 32;

/// How much a term in a section's **heading** outweighs the same term in
/// its body, in the `bm25` ranking.
///
/// The heading chain is already part of the indexed `text`, so indexing
/// `heading_path` as its own column counts a heading term twice. That
/// alone is what separates the section that *is* `D-006` from the one
/// that *mentions* it — measured on the production corpus, one column got
/// 4 of 7 decision identifiers right, two columns got 7 of 7. This weight
/// then decides ranks 2 and 3, where it promotes the sibling pieces of
/// the same decision over unrelated sections citing it; 10.0 and 25.0
/// behave identically, so it sits on a plateau rather than on a tuned
/// edge. Prose queries do not move. Full measurement in
/// `migrations/0065_wiki_sections_fts.sql`.
const LEXICAL_HEADING_WEIGHT: f64 = 4.0;

/// Turn a raw query into an FTS5 `MATCH` expression — `None` when nothing
/// searchable survives (punctuation, emoji, empty string).
///
/// Three properties, each load-bearing:
///
/// **Every term is a quoted phrase.** FTS5's `MATCH` argument is a small
/// query language: bare `AND`/`OR`/`NOT`/`NEAR` are operators, `*` is a
/// prefix, `(`/`)` group, and a stray `"` is a syntax error that fails the
/// whole statement. Quoting each term makes a user typing "`memory OR
/// nothing`" search for the *words*, and makes a malformed expression
/// unconstructible rather than merely unlikely.
///
/// **A word that tokenizes to several tokens becomes a phrase, not
/// several terms.** This is what makes identifiers work. `D-006` splits —
/// under `unicode61`, both here and inside the index — into `d` and `006`,
/// so the phrase `"d 006"` matches only text where those tokens are
/// *adjacent*: the identifier, and not every document containing a stray
/// `d`. Emitting them as two independent terms would match half the
/// corpus.
///
/// **Terms are joined by `OR`, never `AND`.** The result is a *ranking*
/// input, not a filter: a prose turn shares few words with the page that
/// answers it, and `AND` would return nothing. `bm25` already discounts
/// words that appear everywhere, and only the head of the list is
/// consulted, so the loose match costs precision nowhere.
///
/// The split matches `unicode61`'s own rule — a token is a run of Unicode
/// letters and digits, everything else separates — so a term this builds
/// can always be found by the index. Case and diacritics are folded by
/// FTS5 itself, on the query as on the text.
#[must_use]
pub fn lexical_query(raw: &str) -> Option<String> {
    let terms = lexical_terms(raw);
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// The same terms, but **all of them**, and only against the heading
/// column: the expression that asks "is this section *titled* with the
/// query?".
///
/// This is the definition/citation distinction, and it is why the 4x
/// heading weight was not enough on its own. `D-001` cites `D-006` in its
/// body, so it is in the lexical list too — two positions behind the
/// section actually titled `D-006`, which rank fusion cannot recover from
/// when the citing section leads the *vector* list. Verified
/// arithmetically before writing any code: neither a smaller `RRF_K` nor a
/// heavier lexical term flips it, because both are monotone in a rank gap
/// of two. A tier does.
///
/// `AND`, deliberately, where [`lexical_query`] uses `OR`. A heading that
/// contains *every* term of the query is the query's subject; a heading
/// that shares one word with a prose sentence is a coincidence, and
/// promoting on it would make this tier fire on every ordinary turn. A
/// long prose query therefore matches no heading at all — the tier goes
/// quiet exactly where it has nothing to say.
#[must_use]
pub fn lexical_heading_query(raw: &str) -> Option<String> {
    let terms = lexical_terms(raw);
    (!terms.is_empty()).then(|| format!("{{heading_path}} : ({})", terms.join(" AND ")))
}

/// Shared term extraction for both query builders — see [`lexical_query`]
/// for the tokenization rules this implements.
fn lexical_terms(raw: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in raw.split_whitespace() {
        let tokens: Vec<&str> = word
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            continue;
        }
        // Tokens are alphanumeric by construction, so no token can carry
        // the `"` that would close the phrase early.
        let term = format!("\"{}\"", tokens.join(" ").to_lowercase());
        if !terms.contains(&term) {
            terms.push(term);
        }
        if terms.len() == LEXICAL_MAX_TERMS {
            break;
        }
    }
    terms
}

/// Lexical top-`limit` over the sections of `wiki_ids`, best `bm25` first.
///
/// Returns identities only — `(source_path, section_ord)` — because the
/// caller already holds the rows: this pass exists to say *which* sections
/// contain the query's words, and the vector pass it is fused with has
/// their text and their embeddings already.
///
/// ACL is the same wiki-level decision the vector pass makes, applied in
/// the same place (before the scan): a wiki absent from `wiki_ids` cannot
/// contribute a row.
///
/// The index stores no copy of the text (`content='wiki_sections'`), so
/// the join is on `rowid` and the bytes are read once, from the one place
/// they live.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn search_lexical(
    pool: &SqlitePool,
    wiki_ids: &[String],
    query: &str,
    limit: usize,
) -> Result<Vec<(String, i64)>> {
    search_lexical_with(pool, wiki_ids, lexical_query(query), limit).await
}

/// The sections whose **heading** carries every term of the query —
/// the ones the query *names* rather than mentions.
///
/// Same index, same ACL, same shape as [`search_lexical`]; only the
/// `MATCH` expression differs ([`lexical_heading_query`]). Empty on any
/// query whose terms are not all in one heading, which is the normal case
/// for prose.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn search_lexical_headings(
    pool: &SqlitePool,
    wiki_ids: &[String],
    query: &str,
    limit: usize,
) -> Result<Vec<(String, i64)>> {
    search_lexical_with(pool, wiki_ids, lexical_heading_query(query), limit).await
}

async fn search_lexical_with(
    pool: &SqlitePool,
    wiki_ids: &[String],
    match_expr: Option<String>,
    limit: usize,
) -> Result<Vec<(String, i64)>> {
    if wiki_ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let Some(match_expr) = match_expr else {
        return Ok(Vec::new());
    };
    let placeholders = std::iter::repeat_n("?", wiki_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    // The index is unaliased on purpose: `MATCH` and `bm25()` both need
    // the FTS table by its real name, and an alias makes SQLite reject
    // them ("no such column: f").
    //
    // `bm25()` is *negative* — a better match is a smaller number — so
    // plain ascending order is best-first. The weights say a section
    // headed `D-006` beats one that merely cites it; see the migration
    // for what that was measured against.
    let sql = format!(
        r"SELECT s.source_path, s.section_ord
            FROM wiki_sections_fts
            JOIN wiki_sections s ON s.rowid = wiki_sections_fts.rowid
           WHERE wiki_sections_fts MATCH ?
             AND s.wiki_id IN ({placeholders})
           ORDER BY bm25(wiki_sections_fts, {LEXICAL_HEADING_WEIGHT:.1}, 1.0)
           LIMIT ?"
    );
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql).bind(match_expr);
    for id in wiki_ids {
        q = q.bind(id);
    }
    let q = q.bind(i64::try_from(limit).unwrap_or(i64::MAX));
    Ok(q.fetch_all(pool).await?)
}

// ---------- Browser view (no embeddings) ----------

/// One section as the operator's browser sees it — **without** its
/// embedding.
///
/// The vector is ~4 KB per row and useless to a table view, so the
/// listing query leaves it in the DB: rendering a page of sections must
/// not pull megabytes of float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionSummary {
    /// Workdir-relative page path.
    pub source_path: String,
    /// Position on the page.
    pub section_ord: i64,
    /// Containing smart wiki.
    pub wiki_id: String,
    /// Heading chain, when the section sits under one.
    pub heading_path: Option<String>,
    /// The indexed text.
    pub text: String,
    /// First time this position was indexed.
    pub created_at: String,
    /// Last time this position's content changed.
    pub updated_at: String,
    /// Last time it surfaced in a recall top-K.
    pub last_recall_at: Option<String>,
    /// Rolling 30-day recall counter.
    pub recall_count_30d: i64,
}

/// Filters for [`browse`]. All optional, composed in AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowseFilter {
    /// Exact `wiki_id`.
    pub wiki_id: Option<String>,
    /// Substring of `source_path`.
    pub path_contains: Option<String>,
    /// Substring of the indexed text.
    pub text_contains: Option<String>,
    /// Hard cap on rows scanned. `0` = no cap.
    pub limit: usize,
}

/// List sections of the wikis in `wiki_ids`, ordered by wiki, page, then
/// position — the operator's read-only browser over the section index.
///
/// Read access is the caller's to resolve, once per wiki, exactly as in
/// [`find_candidates_in_wikis`]: pass only the ids the viewer may read.
/// An empty `wiki_ids` returns no rows without touching the DB.
///
/// # Errors
///
/// `sqlx::Error`.
pub async fn browse(
    pool: &SqlitePool,
    wiki_ids: &[String],
    filter: &BrowseFilter,
) -> Result<Vec<SectionSummary>> {
    if wiki_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", wiki_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        r#"SELECT source_path, section_ord, wiki_id, heading_path, "text",
                  created_at, updated_at, last_recall_at, recall_count_30d
             FROM wiki_sections
            WHERE wiki_id IN ({placeholders})"#
    );
    let mut binds: Vec<String> = wiki_ids.to_vec();
    if let Some(w) = &filter.wiki_id {
        sql.push_str(" AND wiki_id = ?");
        binds.push(w.clone());
    }
    if let Some(p) = &filter.path_contains {
        sql.push_str(" AND source_path LIKE ? ESCAPE '\\'");
        binds.push(format!("%{}%", escape_like(p)));
    }
    if let Some(t) = &filter.text_contains {
        sql.push_str(" AND \"text\" LIKE ? ESCAPE '\\'");
        binds.push(format!("%{}%", escape_like(t)));
    }
    sql.push_str(" ORDER BY wiki_id ASC, source_path ASC, section_ord ASC");
    if filter.limit > 0 {
        sql.push_str(" LIMIT ?");
        binds.push(filter.limit.to_string());
    }

    let mut q = sqlx::query_as::<_, RawSectionSummary>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let raw = q.fetch_all(pool).await?;
    Ok(raw.into_iter().map(SectionSummary::from).collect())
}

/// Escape the `LIKE` wildcards in operator-supplied filter text, so a
/// literal `%` or `_` searches for itself instead of matching everything.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[derive(sqlx::FromRow)]
struct RawSectionSummary {
    source_path: String,
    section_ord: i64,
    wiki_id: String,
    heading_path: Option<String>,
    text: String,
    created_at: String,
    updated_at: String,
    last_recall_at: Option<String>,
    recall_count_30d: i64,
}

impl From<RawSectionSummary> for SectionSummary {
    fn from(raw: RawSectionSummary) -> Self {
        Self {
            source_path: raw.source_path,
            section_ord: raw.section_ord,
            wiki_id: raw.wiki_id,
            heading_path: raw.heading_path,
            text: raw.text,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            last_recall_at: raw.last_recall_at,
            recall_count_30d: raw.recall_count_30d,
        }
    }
}

impl SectionSummary {
    /// Stable `"<source_path>#<section_ord>"` handle.
    #[must_use]
    pub fn handle(&self) -> String {
        format!("{}#{}", self.source_path, self.section_ord)
    }
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
    /// The wiki's directory slug — the human-facing name an operator
    /// types ("`AcmeSigns`" → `acmesigns`). Mirrored from `_meta.md` so
    /// the per-turn recall can tell when a message names this project.
    pub slug: String,
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
    slug: String,
    owner_id: String,
    shared_with: String,
    project_id: Option<String>,
    wiki_type: String,
}

fn decode_smart_wiki(raw: RawSmartWikiRow) -> Result<SmartWikiRow> {
    Ok(SmartWikiRow {
        wiki_id: raw.wiki_id,
        slug: raw.slug,
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
             (wiki_id, slug, owner_id, shared_with, project_id, wiki_type, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(wiki_id) DO UPDATE SET
             slug        = excluded.slug,
             owner_id    = excluded.owner_id,
             shared_with = excluded.shared_with,
             project_id  = excluded.project_id,
             wiki_type   = excluded.wiki_type,
             updated_at  = excluded.updated_at",
    )
    .bind(&wiki.wiki_id)
    .bind(&wiki.slug)
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
        "SELECT wiki_id, slug, owner_id, shared_with, project_id, wiki_type FROM smart_wikis",
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
        "SELECT wiki_id, slug, owner_id, shared_with, project_id, wiki_type FROM smart_wikis \
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
    async fn browse_filters_compose_and_skip_the_embedding() {
        let pool = pool().await;
        let a = "wikis/alice/proj/auth.md";
        let b = "wikis/alice/proj/billing.md";
        replace_page_sections(
            &pool,
            a,
            &[section(a, 0, "JWT rotation"), section(a, 1, "MFA codes")],
        )
        .await
        .unwrap();
        replace_page_sections(&pool, b, &[section(b, 0, "invoice numbering")])
            .await
            .unwrap();

        let all = browse(&pool, &["alice-proj".to_owned()], &BrowseFilter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        // Ordered by wiki, page, position.
        assert_eq!(all[0].source_path, a);
        assert_eq!(all[0].section_ord, 0);
        assert_eq!(all[2].source_path, b);
        assert_eq!(all[0].handle(), format!("{a}#0"));

        let by_path = browse(
            &pool,
            &["alice-proj".to_owned()],
            &BrowseFilter {
                path_contains: Some("billing".to_owned()),
                ..BrowseFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_path.len(), 1);

        let by_text = browse(
            &pool,
            &["alice-proj".to_owned()],
            &BrowseFilter {
                text_contains: Some("MFA".to_owned()),
                ..BrowseFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].section_ord, 1);

        let capped = browse(
            &pool,
            &["alice-proj".to_owned()],
            &BrowseFilter {
                limit: 2,
                ..BrowseFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(capped.len(), 2);

        // An unreadable wiki set yields nothing without touching the DB.
        assert!(
            browse(&pool, &[], &BrowseFilter::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn browse_treats_like_wildcards_as_literals() {
        let pool = pool().await;
        let page = "wikis/alice/proj/index.md";
        replace_page_sections(
            &pool,
            page,
            &[section(page, 0, "100% coverage"), section(page, 1, "plain")],
        )
        .await
        .unwrap();

        // A bare `%` must search for the character, not match every row.
        let hits = browse(
            &pool,
            &["alice-proj".to_owned()],
            &BrowseFilter {
                text_contains: Some("100%".to_owned()),
                ..BrowseFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("100%"));
    }

    #[tokio::test]
    async fn smart_wiki_registry_upserts_and_reads_back() {
        let pool = pool().await;
        let row = SmartWikiRow {
            wiki_id: "alice-proj".to_owned(),
            slug: "proj".to_owned(),
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

    // ---------- lexical search ----------

    #[test]
    fn lexical_query_quotes_every_term_and_keeps_identifiers_adjacent() {
        // A hyphenated identifier becomes ONE phrase: the two tokens must
        // be adjacent in the text, which is what distinguishes `D-006`
        // from a document containing a stray `d`.
        assert_eq!(lexical_query("D-006").as_deref(), Some(r#""d 006""#));
        // Operator words are quoted, so they are searched, not obeyed.
        assert_eq!(
            lexical_query("memory OR nothing").as_deref(),
            Some(r#""memory" OR "or" OR "nothing""#)
        );
        // Repeats collapse; punctuation and case fall away.
        assert_eq!(
            lexical_query("Deploy, deploy!").as_deref(),
            Some(r#""deploy""#)
        );
        // A `"` cannot reach the expression: it is a separator, not a token.
        assert_eq!(
            lexical_query(r#"say "hi""#).as_deref(),
            Some(r#""say" OR "hi""#)
        );
        // Nothing searchable survives.
        assert_eq!(lexical_query("   ?! —  "), None);
        assert_eq!(lexical_query(""), None);
    }

    #[test]
    fn lexical_heading_query_demands_every_term_in_the_heading() {
        // One term: the identifier case, and the whole point of the tier.
        assert_eq!(
            lexical_heading_query("D-006").as_deref(),
            Some(r#"{heading_path} : ("d 006")"#)
        );
        // Several terms are ANDed — a heading that shares *one* word with
        // a prose sentence is a coincidence, not a definition.
        assert_eq!(
            lexical_heading_query("retry policy").as_deref(),
            Some(r#"{heading_path} : ("retry" AND "policy")"#)
        );
        assert_eq!(lexical_heading_query("  ?!  "), None);
    }

    #[test]
    fn lexical_query_caps_the_term_count() {
        let long = (0..100)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = lexical_query(&long).expect("some");
        assert_eq!(q.matches(" OR ").count(), LEXICAL_MAX_TERMS - 1);
    }

    #[tokio::test]
    async fn search_lexical_finds_the_identifier_and_respects_the_wiki_acl() {
        let pool = pool().await;
        let page = "wikis/alice/proj/decisions.md";
        let other = "wikis/bob/proj/adr.md";
        replace_page_sections(
            &pool,
            page,
            &[
                NewSection {
                    text: "Decisions > D-001. We chose the queue. Superseded in part by D-006."
                        .to_owned(),
                    ..section(page, 0, "")
                },
                NewSection {
                    text: "Decisions > D-006. Retry with backoff, then dead-letter.".to_owned(),
                    ..section(page, 1, "")
                },
                NewSection {
                    text: "Changelog. Fixed a d bug in the renderer.".to_owned(),
                    ..section(page, 2, "")
                },
            ],
        )
        .await
        .unwrap();
        replace_page_sections(
            &pool,
            other,
            &[NewSection {
                wiki_id: "bob-proj".to_owned(),
                text: "ADR-006 cross-references D-006 of the other project.".to_owned(),
                ..section(other, 0, "")
            }],
        )
        .await
        .unwrap();

        let readable = vec!["alice-proj".to_owned()];
        let hits = search_lexical(&pool, &readable, "D-006", 10).await.unwrap();
        // The section that *is* D-006 leads; the one that merely cites it
        // follows; the "a d bug" section is absent, because the phrase
        // needs the two tokens adjacent; Bob's wiki never enters.
        assert_eq!(
            hits,
            vec![(page.to_owned(), 1), (page.to_owned(), 0)],
            "lexical order"
        );

        // No readable wiki, no limit, no searchable term: all empty, and
        // none of them touches the index.
        assert!(
            search_lexical(&pool, &[], "D-006", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            search_lexical(&pool, &readable, "D-006", 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            search_lexical(&pool, &readable, "?!", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_section_titled_with_an_identifier_beats_a_shorter_one_citing_it() {
        // The failure this index was rebuilt to fix, reduced to two rows.
        // Plain bm25 rewards the short block that *mentions* `D-006` over
        // the long one that is *titled* with it — on the production
        // corpus that lost 3 of 7 decision identifiers. Indexing the
        // heading chain as its own weighted column is what separates
        // "this section is D-006" from "this section refers to D-006".
        let pool = pool().await;
        let page = "wikis/alice/proj/decisions.md";
        let cites = "Decision log > D-001 — queue. Superseded in part by D-006.";
        let titled = format!(
            "Decision log > D-006 — a picture on screen is a preview. {}",
            "The renderer never writes the file it shows. ".repeat(12)
        );
        replace_page_sections(
            &pool,
            page,
            &[
                NewSection {
                    heading_path: Some("Decision log > D-001 — queue".to_owned()),
                    text: cites.to_owned(),
                    ..section(page, 0, "")
                },
                NewSection {
                    heading_path: Some(
                        "Decision log > D-006 — a picture on screen is a preview".to_owned(),
                    ),
                    text: titled,
                    ..section(page, 1, "")
                },
            ],
        )
        .await
        .unwrap();

        let hits = search_lexical(&pool, &["alice-proj".to_owned()], "D-006", 10)
            .await
            .unwrap();
        assert_eq!(
            hits.first(),
            Some(&(page.to_owned(), 1)),
            "the section headed D-006 must lead the one that cites it"
        );
    }

    #[tokio::test]
    async fn the_lexical_index_tracks_edits_and_deletions() {
        let pool = pool().await;
        let page = "wikis/alice/proj/notes.md";
        let readable = vec!["alice-proj".to_owned()];
        replace_page_sections(&pool, page, &[section(page, 0, "the pineapple protocol")])
            .await
            .unwrap();
        assert_eq!(
            search_lexical(&pool, &readable, "pineapple", 10)
                .await
                .unwrap(),
            vec![(page.to_owned(), 0)]
        );

        // An in-place edit keeps the row's identity — the UPDATE trigger,
        // not a delete/insert pair, is what has to re-index it.
        replace_page_sections(&pool, page, &[section(page, 0, "the rhubarb protocol")])
            .await
            .unwrap();
        assert!(
            search_lexical(&pool, &readable, "pineapple", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            search_lexical(&pool, &readable, "rhubarb", 10)
                .await
                .unwrap(),
            vec![(page.to_owned(), 0)]
        );

        // And a dropped page leaves nothing behind to be found.
        drop_page_sections(&pool, page).await.unwrap();
        assert!(
            search_lexical(&pool, &readable, "rhubarb", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
