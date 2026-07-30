// SPDX-License-Identifier: AGPL-3.0-or-later
//! `fact_index` — SQLite-backed region-level index.
//!
//! One row per region delimited by `{{f=<UUIDv7>}}…{{/}}` markers in the
//! memory-wiki filesystem ([memory model](../../../docs/concepts/memory-model.md)).
//! The `.md` files stay authoritative for the prose; for the **region
//! ACL** the DB is the authoritative source on the read side — redaction
//! resolves it by fact key via [`page_acl_map`], with the inline marker
//! attributes as the transition fallback
//! (redaction policy).
//!
//! See the table DDL in
//! [`migrations/0001_fact_index.sql`](../../migrations/0001_fact_index.sql)
//! and the schema reference in
//! engine DB and migrations.
//!
//! This module is the data-access layer: insert, find-active, mark
//! superseded/forgotten, find-by-id, drop-by-source-path. The full
//! recall pipeline (semantic similarity ranking, multi-modal recall,
//! REM jobs) builds on top of these primitives.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqlitePool};
use thiserror::Error;

use crate::acl::{FactAclMap, RegionAcl};
use crate::types::{FactId, Principal};

/// Errors raised by the fact-index layer.
#[derive(Debug, Error)]
pub enum FactIndexError {
    /// Underlying `SQLite` error.
    #[error("fact_index db: {0}")]
    Db(#[from] sqlx::Error),

    /// JSON serialization failure on `allow_ids` / `topics`.
    #[error("fact_index json: {0}")]
    Json(#[from] serde_json::Error),

    /// The embedding column was stored as a length that is not a
    /// multiple of 4 bytes (i.e. cannot be decoded as `f32` little-endian).
    #[error("fact_index embedding blob length {0} is not divisible by 4")]
    InvalidEmbeddingBlob(usize),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, FactIndexError>;

// ---------- FactIndexRow ----------

/// One row of the `fact_index` table, with the wire types decoded into
/// Rust-native representations (vectors, principal lists, optional
/// timestamps).
#[derive(Debug, Clone, PartialEq)]
pub struct FactIndexRow {
    /// Stable `UUIDv7` fact identifier.
    pub fact_id: FactId,
    /// `wiki_id` of the containing wiki node.
    pub wiki_id: String,
    /// Path of the source `.md` file, relative to workdir.
    pub source_path: String,
    /// Byte offset of the opening `{{f=…}}` marker (inclusive). `None`
    /// when the row was inserted before offset capture was wired (a
    /// later migration may add it).
    pub region_start: Option<i64>,
    /// Byte offset one past the closing `{{/}}` marker. Same nullability
    /// rationale as [`Self::region_start`].
    pub region_end: Option<i64>,
    /// Region body text (no markers). Stored verbatim so a re-embed run
    /// or an audit query can read the indexed text without touching the
    /// filesystem.
    pub text: String,
    /// Embedding vector. Decoded from the BLOB column.
    pub embedding: Vec<f32>,
    /// The fact's **subject** — who/what it is *about* (`user:<id>` |
    /// `group:<id>` | `global`). NOT its author (that is `sender_id`) and
    /// NOT its audience (that is `allow_ids`); "owner" because the subject
    /// governs their own fact's ACL. Persisted in the `owner_id` column.
    /// See the engineering wiki (`concepts/identity-and-acl.md`).
    pub owner_id: Principal,
    /// Additional principals the region's `allow=` extension grants
    /// read access to (possibly empty).
    pub allow_ids: Vec<Principal>,
    /// Cross-user attribution — the principal who authored the region.
    /// Always populated on the write path (equal to `owner_id` for a
    /// self-authored fact); `None` only on legacy rows with unknown
    /// provenance.
    pub sender_id: Option<Principal>,
    /// Optional fact taxonomy hint (`bio`, `preference`, …).
    pub fact_type: Option<String>,
    /// Free-form topic tags, denormalized for SQL search.
    pub topics: Vec<String>,
    /// Wall-clock at first insert (ISO 8601, UTC).
    pub created_at: String,
    /// Wall-clock at last meaningful update (ISO 8601, UTC).
    pub updated_at: String,
    /// Set to a timestamp when `wiki_supersede` retires the row.
    pub superseded_at: Option<String>,
    /// `fact_id` of the row that replaced this one in the supersedence
    /// chain.
    pub superseded_by: Option<FactId>,
    /// `fact_id` of the fact that replaced this one, when the closure knew
    /// it (a contradiction satellite inherits the seed's superseding fact;
    /// a completion closure points at its evidence). Rides a **live**
    /// closed row — unlike [`Self::superseded_by`], which is welded to the
    /// [`Self::superseded_at`] tombstone — so the compile feed can point
    /// the reader at the current truth ("no longer current — today see …").
    pub successor_fact_id: Option<FactId>,
    /// Set when `wiki_forget` (or filesystem removal) tombstones the row.
    pub deleted_at: Option<String>,
    /// Free-form reason matching engine DB and migrations:
    /// `filesystem_removed`, `user_request`, `gdpr_erasure`, …
    pub deleted_reason: Option<String>,
    /// Last wall-clock the row appeared in `wiki_recall` / `wiki_search`
    /// top-K. Used by REM for rolling recall counters.
    pub last_recall_at: Option<String>,
    /// Rolling 30-day recall counter maintained by REM. Defaults to 0.
    pub recall_count_30d: i64,
    /// Start of the fact's validity interval (ISO 8601). `None` = unknown /
    /// "since forever". Part of the per-fact validity model
    /// ([memory model](../../../docs/concepts/memory-model.md)).
    /// Additive + inert until the writer populates it.
    pub valid_from: Option<String>,
    /// End of the validity interval (ISO 8601). `None` = OPEN ("true now, no
    /// horizon" — the old "knowledge" regime); a set value is what "state"
    /// used to mean. At recall this is a soft-down-rank SIGNAL once
    /// `now >= valid_to`, never a hard filter.
    pub valid_to: Option<String>,
    /// Why `valid_to` was closed. `None` while the fact is alive; stamped on
    /// closure from the [`decay`] vocabulary (enforced at the producer, like
    /// [`Self::deleted_reason`] — the column stays free TEXT so future
    /// closure kinds need no migration).
    pub decay_reason: Option<String>,
    /// Per-fact SALIENCE: how always-relevant the fact
    /// is to the owner. `None` = unspecified (treated as `normal`); the closed
    /// set is `high | normal | low`, deduced by the ingest classifier (no
    /// hardcoded gate, enforced at the producer like [`Self::fact_type`]).
    /// `high` = "must be known in every interaction" → routed to the actor-wiki
    /// `index.md` base context.
    pub salience: Option<String>,
    /// The page the ingest classifier
    /// proposed this fact be placed on (a slug or `.md` path). A *hint*, not the
    /// fact's home: the compilation plan is the authority on placement, and the
    /// REM Cartografo may re-home the fact. Read only in the LIGHT cadence, only
    /// for a not-yet-planned fact, so the light dream can settle a fact on its
    /// ingest page without re-running the strong-model Cartografo. `None`
    /// = the classifier proposed nothing (older rows, the direct path).
    pub target_page: Option<String>,
    /// Proposed page writing style (closed palette `prosa` | `prosa-tecnica` |
    /// `lista`) that seeds a freshly-created page's testata. `None` =
    /// unproposed. See [`Self::target_page`].
    pub style: Option<String>,
    /// Proposed "cosa ci va dentro" one-liner that seeds the page's
    /// testata description. `None` = unproposed. See [`Self::target_page`].
    pub page_description: Option<String>,
    /// Provenance of an extracted fact: the media-catalog id or URL of the
    /// source document (document ingest).
    /// `None` for ordinary conversational captures. DB-authoritative
    /// metadata — audit/citation surface, never rendered into the page.
    pub source_ref: Option<String>,
    /// Project-wiki pages this fact's originating turn authored, as plain
    /// `[[wiki_id/page]]` wikilinks (a smart consumer carried them in via
    /// `wiki_ingest_message`'s `metadata.authored_refs`). Lets
    /// consolidation record a **reference** to the project page instead of
    /// re-storing its body — the "link, don't duplicate" provenance tube
    /// (roadmap group 17). Empty for a pure-standard capture.
    pub authored_refs: Vec<String>,
}

impl FactIndexRow {
    /// True when this fact belongs to the owner's **identity core** — the
    /// always-on base context that identifies *who someone is and how they
    /// relate to others* (`fact_type = "bio"` AND `salience = "high"`).
    ///
    /// The identity core is the small always-on set the ingest classifier
    /// routes to the owner's `index.md`: name/aliases, **role(s) and the
    /// people they are tied to (relations)**, birthdate, place, contacts.
    /// It is deliberately **stable** — automatic background reorganisation
    /// (the REM dedup revisor) must never silently retire one of these
    /// facts; only an explicit correction (a user-driven supersede, or a
    /// dashboard edit) may change it. Callers on automatic-supersede paths
    /// gate on this predicate; the explicit paths do not.
    #[must_use]
    pub fn is_identity_core(&self) -> bool {
        self.fact_type.as_deref() == Some("bio") && self.salience.as_deref() == Some("high")
    }
}

/// Insert payload — the subset of [`FactIndexRow`] a fresh capture
/// needs to supply.
///
/// The DB fills `created_at` / `updated_at`, leaves supersedence and
/// tombstone columns NULL, and zeroes the recall counters.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFact {
    /// Pre-generated `UUIDv7` (capture generates it client-side so the id
    /// is available before any DB or filesystem write commits).
    pub fact_id: FactId,
    /// Containing wiki.
    pub wiki_id: String,
    /// Source file path, relative to workdir.
    pub source_path: String,
    /// Byte offset of the opening marker, when known.
    pub region_start: Option<i64>,
    /// Byte offset one past the closing marker, when known.
    pub region_end: Option<i64>,
    /// Region body verbatim (no markers).
    pub text: String,
    /// Pre-computed embedding vector.
    pub embedding: Vec<f32>,
    /// The fact's **subject** — who/what it is *about* (not its author
    /// `sender_id`, not its audience `allow_ids`).
    pub owner_id: Principal,
    /// `allow=` extension list (possibly empty).
    pub allow_ids: Vec<Principal>,
    /// Cross-user attribution.
    pub sender_id: Option<Principal>,
    /// Optional fact taxonomy hint.
    pub fact_type: Option<String>,
    /// Topic tags.
    pub topics: Vec<String>,
    /// Start of the validity interval (ISO 8601) when the producer knows it.
    /// `None` = unknown / open-start. See [`FactIndexRow::valid_from`].
    pub valid_from: Option<String>,
    /// End of the validity interval (ISO 8601) when known at capture. `None`
    /// = OPEN ("true now"). See [`FactIndexRow::valid_to`]. (`NewFact` carries
    /// no `decay_reason`: a fresh fact is alive, so insert leaves it NULL.)
    pub valid_to: Option<String>,
    /// Per-fact salience the producer deduced. See
    /// [`FactIndexRow::salience`]. `None` = unspecified.
    pub salience: Option<String>,
    /// Ingest-proposed placement page. See
    /// [`FactIndexRow::target_page`]. `None` = unproposed.
    pub target_page: Option<String>,
    /// Ingest-proposed page style. See [`FactIndexRow::style`].
    pub style: Option<String>,
    /// Ingest-proposed page description. See
    /// [`FactIndexRow::page_description`].
    pub page_description: Option<String>,
    /// Source-document provenance. See [`FactIndexRow::source_ref`].
    /// `None` for conversational captures (the overwhelmingly common case).
    pub source_ref: Option<String>,
    /// Provenance breadcrumbs for the turn. See
    /// [`FactIndexRow::authored_refs`]. Empty for a pure-standard capture.
    pub authored_refs: Vec<String>,
}

// ---------- Embedding (de)serialization ----------

/// Encode a vector as little-endian `f32` bytes for storage in the
/// `embedding` BLOB column.
///
/// Little-endian matches the byte order of every architecture mwe-mcp
/// supports (`x86_64`, `aarch64`, …); we still write it explicitly so a
/// cross-arch backup restore is well-defined.
#[must_use]
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode an embedding BLOB back to a `f32` vector.
///
/// # Errors
///
/// [`FactIndexError::InvalidEmbeddingBlob`] when `bytes.len() % 4 != 0`.
pub fn decode_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(FactIndexError::InvalidEmbeddingBlob(bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    let mut buf = [0u8; 4];
    for chunk in bytes.chunks_exact(4) {
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

// ---------- Helpers for principal JSON ----------

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct PrincipalWire(String);

pub(crate) fn principals_to_json(
    ps: &[Principal],
) -> std::result::Result<String, serde_json::Error> {
    let wire: Vec<PrincipalWire> = ps.iter().map(|p| PrincipalWire(p.to_string())).collect();
    serde_json::to_string(&wire)
}

pub(crate) fn principals_from_json(s: &str) -> std::result::Result<Vec<Principal>, String> {
    let wire: Vec<PrincipalWire> =
        serde_json::from_str(s).map_err(|e| format!("allow_ids JSON: {e}"))?;
    wire.into_iter()
        .map(|w| w.0.parse::<Principal>().map_err(|e| e.to_string()))
        .collect()
}

fn topics_to_json(ts: &[String]) -> std::result::Result<String, serde_json::Error> {
    serde_json::to_string(ts)
}

fn topics_from_json(s: &str) -> std::result::Result<Vec<String>, serde_json::Error> {
    serde_json::from_str(s)
}

// ---------- Insert ----------

/// Insert a fresh fact row. The caller is responsible for ensuring the
/// `fact_id` is unique — a duplicate surfaces as a unique-constraint
/// violation via the wrapped `sqlx::Error`.
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `allow_ids` / `topics`.
pub async fn insert(pool: &SqlitePool, fact: &NewFact) -> Result<()> {
    let n = insert_with(pool, fact, false).await?;
    debug_assert_eq!(n, 1, "plain insert must affect exactly one row");
    Ok(())
}

/// Atomically replace every `fact_index` row at `source_path` with `facts`
/// — drop the page's existing rows and insert the fresh set in **one
/// transaction**.
///
/// This is the write half of smart-wiki section indexing
/// ([`crate::reindex`]): a smart page is "drop the page's section rows,
/// insert the freshly chunked ones". Doing the drop and the inserts in a
/// single transaction is what makes **concurrent** reindexers of the same
/// page converge to one clean set instead of accumulating duplicate rows:
/// `SQLite` serializes writers, so a second reindex's drop catches the
/// first's just-committed rows rather than interleaving between a separate
/// drop and a separate insert. Embeddings must already be computed and
/// carried in `facts` — the transaction does only fast DB work, never the
/// slow embed I/O (holding a write transaction across a network embed
/// would block every other writer).
///
/// Returns `(dropped, inserted)`.
///
/// # Errors
///
/// `sqlx::Error` (including a `fact_id` unique-constraint collision) +
/// JSON serialization failures on `allow_ids` / `topics`; the transaction
/// is rolled back on any error.
pub async fn replace_source_path_rows(
    pool: &SqlitePool,
    source_path: &str,
    facts: &[NewFact],
) -> Result<(u64, u64)> {
    let mut tx = pool.begin().await?;
    let dropped = sqlx::query("DELETE FROM fact_index WHERE source_path = ?")
        .bind(source_path)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    for fact in facts {
        insert_with(&mut *tx, fact, false).await?;
    }
    tx.commit().await?;
    let inserted = u64::try_from(facts.len()).unwrap_or(u64::MAX);
    Ok((dropped, inserted))
}

/// Insert a fresh fact row, but silently skip when a row with the same
/// `fact_id` already exists.
///
/// The reindex pipeline uses this primitive to close the watcher race
/// where `wiki_capture` has just inserted a row and committed the file
/// to disk, the filesystem watcher fires an event for that same file
/// before the next caller had a chance to observe the inserted row, and
/// the reindex sees the marker on disk while its
/// `find_active_by_source_path` snapshot is still empty (or only
/// contains a tombstoned / superseded twin). A plain [`insert`] would
/// surface `UNIQUE constraint failed: fact_index.fact_id` and abort the
/// reconciliation; this variant degrades to a no-op so the reindex
/// finishes cleanly. Any structural drift across the surviving row is
/// picked up by the next safety-net sweep.
///
/// Returns the number of rows affected: `1` when the row was freshly
/// inserted, `0` when a row with the same `fact_id` already existed
/// (active, superseded, or tombstoned — the conflict is on the primary
/// key, not on the active-row subset).
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `allow_ids` / `topics`.
pub async fn insert_if_absent(pool: &SqlitePool, fact: &NewFact) -> Result<u64> {
    insert_with(pool, fact, true).await
}

/// Insert one row on any executor — a pool *or* an open transaction — so
/// [`replace_source_path_rows`] can drop-and-insert atomically while the
/// plain [`insert`] / [`insert_if_absent`] helpers run on the pool. See
/// [`insert_if_absent`] for the `ignore_conflict` semantics.
async fn insert_with<'e, E>(executor: E, fact: &NewFact, ignore_conflict: bool) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let now = chrono::Utc::now().to_rfc3339();
    let embedding_dim = i64::try_from(fact.embedding.len()).unwrap_or(i64::MAX);
    let blob = encode_embedding(&fact.embedding);
    let allow_json = principals_to_json(&fact.allow_ids)?;
    let topics_json = topics_to_json(&fact.topics)?;
    // `topics_to_json` is a generic Vec<String> → JSON serializer; reused
    // here for the group-17 provenance breadcrumbs (same shape as topics).
    let authored_refs_json = topics_to_json(&fact.authored_refs)?;
    let sender = fact.sender_id.as_ref().map(ToString::to_string);
    let owner = fact.owner_id.to_string();

    // `ON CONFLICT(fact_id) DO NOTHING` rather than `INSERT OR IGNORE`:
    // both work on SQLite, but the explicit conflict-target form makes
    // intent unambiguous at the SQL site (the next reader sees that we
    // only want to absorb a primary-key collision, not silently drop
    // NOT NULL / CHECK failures the way `OR IGNORE` would).
    let sql = if ignore_conflict {
        r#"INSERT INTO fact_index (
            fact_id, wiki_id, source_path, region_start, region_end, "text",
            embedding, embedding_dim, owner_id, allow_ids, sender_id,
            fact_type, topics, created_at, updated_at,
            valid_from, valid_to, target_page, style, page_description,
            salience, source_ref, authored_refs, recall_count_30d
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
        ON CONFLICT(fact_id) DO NOTHING"#
    } else {
        r#"INSERT INTO fact_index (
            fact_id, wiki_id, source_path, region_start, region_end, "text",
            embedding, embedding_dim, owner_id, allow_ids, sender_id,
            fact_type, topics, created_at, updated_at,
            valid_from, valid_to, target_page, style, page_description,
            salience, source_ref, authored_refs, recall_count_30d
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)"#
    };

    let res = sqlx::query(sql)
        .bind(fact.fact_id.as_str())
        .bind(&fact.wiki_id)
        .bind(&fact.source_path)
        .bind(fact.region_start)
        .bind(fact.region_end)
        .bind(&fact.text)
        .bind(&blob)
        .bind(embedding_dim)
        .bind(&owner)
        .bind(&allow_json)
        .bind(&sender)
        .bind(&fact.fact_type)
        .bind(&topics_json)
        .bind(&now)
        .bind(&now)
        .bind(&fact.valid_from)
        .bind(&fact.valid_to)
        .bind(&fact.target_page)
        .bind(&fact.style)
        .bind(&fact.page_description)
        .bind(&fact.salience)
        .bind(&fact.source_ref)
        .bind(&authored_refs_json)
        .execute(executor)
        .await?;
    Ok(res.rows_affected())
}

// ---------- Lifecycle mutations ----------

/// Canonical `decay_reason` values — WHY a fact's validity window closed.
///
/// The column stays free `TEXT` (a future closure kind — e.g. the organic
/// forgetting group's condensation — needs no migration), but every writer
/// in this codebase stamps one of these, so recall/REM/dashboard can key on
/// them. **Expiry stamps nothing**: a `valid_to` in the past *is* the
/// expiry — a reason would only restate the timestamp.
pub mod decay {
    /// A consumable intention was spent: the milk was bought, the film was
    /// watched. Stamped by the ingest/REM completion triggers.
    pub const COMPLETED: &str = "completed";
    /// The user took the fact back — a relayed forget/abandon gesture
    /// ("forget what I told you about…", "I have given up on the
    /// project"). Stamped by the ingest closure path.
    pub const RETRACTED: &str = "retracted";
    /// A later fact superseded this one ([`super::mark_superseded`] stamps
    /// it on the predecessor as part of the supersede chokepoint).
    pub const CONTRADICTED: &str = "contradicted";
}

/// Mark a fact as superseded by `new_fact_id`, stamping `superseded_at`
/// and bumping `updated_at`.
///
/// The supersede is also the **contradiction closure** of the per-fact
/// validity model: the same UPDATE closes the predecessor's window
/// (`valid_to` = now, but only when still open — an earlier concrete end,
/// e.g. a dated commitment, is never extended) and stamps
/// `decay_reason` to [`decay::CONTRADICTED`] (only when no reason is
/// already recorded). One chokepoint serves both the direct path
/// (`wiki_supersede`) and the buffered path (the light dream applies the
/// staged supersede hint through this same function).
///
/// Returns the number of rows touched (0 when `old_fact_id` is unknown).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_superseded(
    pool: &SqlitePool,
    old_fact_id: &FactId,
    new_fact_id: &FactId,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET superseded_at = ?, superseded_by = ?, updated_at = ?,
                valid_to = COALESCE(valid_to, ?),
                decay_reason = COALESCE(decay_reason, ?)
          WHERE fact_id = ? AND superseded_at IS NULL AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(new_fact_id.as_str())
    .bind(&now)
    .bind(&now)
    .bind(decay::CONTRADICTED)
    .bind(old_fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Snapshot of a fact's validity fields the moment a closure overwrote
/// them — the revert payload of the act-first closure receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedValidity {
    /// `valid_to` before the closure (`None` = the window was open).
    pub prev_valid_to: Option<String>,
    /// `decay_reason` before the closure (`None` in the normal case).
    pub prev_decay_reason: Option<String>,
    /// `successor_fact_id` before the closure (`None` in the normal case —
    /// only a re-closure overwrites an earlier pointer).
    pub prev_successor_fact_id: Option<FactId>,
}

/// Close a fact's validity window: stamp `valid_to` + `decay_reason` and
/// bump `updated_at`.
///
/// The write half of the **closure verb** ("ingest closes the validity of
/// existing facts") shared by the relayed forget gesture and the
/// completion trigger. The fact row itself stays alive — closure is a
/// validity statement, never a tombstone — and the page recompiles on the
/// next dream because the validity fields are part of the page
/// fingerprint.
///
/// `successor` is the fact that replaced this one, when the closer knows
/// it (the contradiction sweep passes the seed's superseding fact, the
/// completion sweep its evidence). `Some` stamps `successor_fact_id`;
/// `None` leaves any earlier pointer untouched.
///
/// Returns the previous values for the receipt's revert payload, or
/// `None` when `fact_id` has no active row (unknown or tombstoned) — the
/// caller skips the closure rather than failing the turn.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn close_validity(
    pool: &SqlitePool,
    fact_id: &FactId,
    valid_to: &str,
    reason: &str,
    successor: Option<&FactId>,
) -> Result<Option<ClosedValidity>> {
    let Some(row) = find_by_id(pool, fact_id).await? else {
        return Ok(None);
    };
    if row.deleted_at.is_some() {
        return Ok(None);
    }
    let prev = ClosedValidity {
        prev_valid_to: row.valid_to,
        prev_decay_reason: row.decay_reason,
        prev_successor_fact_id: row.successor_fact_id,
    };
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE fact_index
            SET valid_to = ?, decay_reason = ?,
                successor_fact_id = COALESCE(?, successor_fact_id),
                updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(valid_to)
    .bind(reason)
    .bind(successor.map(FactId::as_str))
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(prev))
}

/// Restore a fact's validity fields from a [`ClosedValidity`] snapshot —
/// the revert half of the closure verb.
///
/// Returns the number of rows touched (0 when the fact no longer has an
/// active row).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn restore_validity(
    pool: &SqlitePool,
    fact_id: &FactId,
    prev_valid_to: Option<&str>,
    prev_decay_reason: Option<&str>,
    prev_successor_fact_id: Option<&str>,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET valid_to = ?, decay_reason = ?, successor_fact_id = ?,
                updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(prev_valid_to)
    .bind(prev_decay_reason)
    .bind(prev_successor_fact_id)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Snapshot of a fact's validity *interval* the moment a date correction
/// overwrote it — the revert payload of the act-first `validity_edit`
/// receipt.
///
/// Distinct from [`ClosedValidity`]: an edit corrects the *bounds*
/// (`valid_from` / `valid_to`) and never touches `decay_reason` — a date
/// correction is not a closure, so a closed fact whose dates were merely
/// fixed keeps its decay stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevValidity {
    /// `valid_from` before the edit.
    pub prev_valid_from: Option<String>,
    /// `valid_to` before the edit.
    pub prev_valid_to: Option<String>,
}

/// Correct a fact's validity *interval*: set `valid_from` and/or
/// `valid_to` and bump `updated_at`, **leaving `decay_reason` untouched**.
///
/// The write half of the **validity-edit verb** ("the owner corrects a
/// stored fact's dates from chat") — the sibling of the closure verb, but
/// for a *correction* rather than a completion/retraction. A `Some(value)`
/// SETS that bound; a `None` LEAVES the bound unchanged (the COALESCE is
/// done in Rust against the snapshot, so an omitted bound is a no-op, not
/// a wipe). The fact row itself stays alive — an edit is never a tombstone
/// — and the page recompiles on the next dream because the validity fields
/// are part of the page fingerprint.
///
/// Returns the previous interval for the receipt's revert payload, or
/// `None` when `fact_id` has no active row (unknown or tombstoned) — the
/// caller skips the edit rather than failing the turn.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn set_validity(
    pool: &SqlitePool,
    fact_id: &FactId,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> Result<Option<PrevValidity>> {
    let Some(row) = find_by_id(pool, fact_id).await? else {
        return Ok(None);
    };
    if row.deleted_at.is_some() {
        return Ok(None);
    }
    let prev = PrevValidity {
        prev_valid_from: row.valid_from.clone(),
        prev_valid_to: row.valid_to.clone(),
    };
    // COALESCE-in-Rust: an omitted bound keeps its previous value, never
    // wipes it. `decay_reason` is deliberately NOT in the SET list — a
    // date correction is not a closure.
    let new_from = valid_from.map_or(row.valid_from, |v| Some(v.to_owned()));
    let new_to = valid_to.map_or(row.valid_to, |v| Some(v.to_owned()));
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE fact_index
            SET valid_from = ?, valid_to = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(&new_from)
    .bind(&new_to)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(prev))
}

/// Restore a fact's validity *interval* from a [`PrevValidity`] snapshot —
/// the revert half of the validity-edit verb. Sets BOTH bounds back and
/// leaves `decay_reason` untouched.
///
/// Returns the number of rows touched (0 when the fact no longer has an
/// active row).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn restore_validity_interval(
    pool: &SqlitePool,
    fact_id: &FactId,
    prev_valid_from: Option<&str>,
    prev_valid_to: Option<&str>,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET valid_from = ?, valid_to = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(prev_valid_from)
    .bind(prev_valid_to)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Snapshot of a fact's ACL columns the moment an ACL change overwrote
/// them — the revert payload of the act-first `acl_change` receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevAcl {
    /// `owner_id` before the change.
    pub prev_owner_id: Principal,
    /// `allow_ids` before the change.
    pub prev_allow_ids: Vec<Principal>,
    /// `sender_id` before the change.
    pub prev_sender_id: Option<Principal>,
}

/// Replace a fact's ACL columns: set `owner_id`, `allow_ids`, and
/// `sender_id`, and bump `updated_at`.
///
/// The write half of the **acl-change verb** ("the owner changes who can
/// read a stored fact from chat"). The fact row stays alive — an ACL
/// change is never a tombstone — and the page recompiles on the next
/// dream because the per-fact ACL is part of the render's authoritative
/// resolution.
///
/// Returns the previous ACL for the receipt's revert payload, or `None`
/// when `fact_id` has no active row (unknown or tombstoned) — the caller
/// skips the change rather than failing the turn.
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `allow_ids`.
pub async fn set_acl(
    pool: &SqlitePool,
    fact_id: &FactId,
    owner: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) -> Result<Option<PrevAcl>> {
    let Some(row) = find_by_id(pool, fact_id).await? else {
        return Ok(None);
    };
    if row.deleted_at.is_some() {
        return Ok(None);
    }
    let prev = PrevAcl {
        prev_owner_id: row.owner_id,
        prev_allow_ids: row.allow_ids,
        prev_sender_id: row.sender_id,
    };
    let allow_json = principals_to_json(allow)?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE fact_index
            SET owner_id = ?, allow_ids = ?, sender_id = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(owner.to_string())
    .bind(&allow_json)
    .bind(sender.map(ToString::to_string))
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(prev))
}

/// Restore a fact's ACL columns from a [`PrevAcl`] snapshot — the revert
/// half of the acl-change verb.
///
/// Returns the number of rows touched (0 when the fact no longer has an
/// active row).
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `allow_ids`.
pub async fn restore_acl(
    pool: &SqlitePool,
    fact_id: &FactId,
    owner: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) -> Result<u64> {
    let allow_json = principals_to_json(allow)?;
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET owner_id = ?, allow_ids = ?, sender_id = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(owner.to_string())
    .bind(&allow_json)
    .bind(sender.map(ToString::to_string))
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Stamp `decay_reason` on a fact that already carries its closing
/// `valid_to`.
///
/// The promotion tail of a closure that landed while the capture was
/// still buffered: the buffer stages the reason, the insert keeps its
/// fresh-fact invariant, and this stamps the WHY right after.
///
/// Returns the number of rows touched.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn stamp_decay_reason(pool: &SqlitePool, fact_id: &FactId, reason: &str) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET decay_reason = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(reason)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Tombstone a fact with the given reason (`user_request`,
/// `filesystem_removed`, `gdpr_erasure`, …).
///
/// Returns the number of rows touched. Idempotent: re-calling on an
/// already-deleted row is a 0 update.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_forgotten(pool: &SqlitePool, fact_id: &FactId, reason: &str) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET deleted_at = ?, deleted_reason = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Un-tombstone a fact: clear `deleted_at` / `deleted_reason` so the row
/// rejoins the active set.
///
/// The exact inverse of [`mark_forgotten`], it is the per-fact primitive a
/// `bundle` revert ([`crate::bundle::revert_bundle`]) uses to restore a fact
/// the deleter tombstoned when a governed page deletion is undone. The fact's
/// text + ACL never left the row (tombstoning only stamps the two columns), so
/// the next compile re-renders its marker on the page the row still points at.
///
/// Guards on `deleted_at IS NOT NULL` so re-restoring a live row is a 0 update
/// (idempotent), and returns the number of rows touched.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn restore_forgotten(pool: &SqlitePool, fact_id: &FactId) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET deleted_at = NULL, deleted_reason = NULL, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NOT NULL",
    )
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Tombstone a fact with the given reason **only if the row still
/// claims to live at `source_path`**.
///
/// The orphan sweep's variant of [`mark_forgotten`]: a reindex pass
/// observes "marker missing from `<file>`" against a row snapshot that
/// may be stale — a concurrent promote/compile can have legitimately
/// repointed the row to another page between the snapshot and this
/// call. Guarding the UPDATE on `source_path` makes the tombstone a
/// no-op in that case, so only a fact whose row *currently* lives on
/// the swept page can be forgotten by that page's sweep.
///
/// Returns the number of rows touched. Idempotent.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_forgotten_at(
    pool: &SqlitePool,
    fact_id: &FactId,
    source_path: &str,
    reason: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET deleted_at = ?, deleted_reason = ?, updated_at = ?
          WHERE fact_id = ? AND deleted_at IS NULL AND source_path = ?",
    )
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(fact_id.as_str())
    .bind(source_path)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Tombstone **every still-active fact** in `wiki_id` with `reason`.
///
/// The bulk sibling of [`mark_forgotten`], used by the operator
/// wiki-delete path ([`crate::wiki_delete`]) to clear a whole wiki's
/// facts from recall in one statement. Superseded / already-tombstoned
/// rows are left untouched (the `deleted_at IS NULL` guard), so the call
/// is idempotent.
///
/// Returns the number of rows touched.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_forgotten_in_wiki(pool: &SqlitePool, wiki_id: &str, reason: &str) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET deleted_at = ?, deleted_reason = ?, updated_at = ?
          WHERE wiki_id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(wiki_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Reassign a removed principal's facts to their wiki's scope principal (23d).
///
/// Substitute every active fact's dangling `sender_id` — equal to the
/// just-removed principal `gone` — with that fact's wiki **scope principal**
/// ([the write-authority model](../../../docs/concepts/identity-and-acl.md)), so a contribution
/// outlives its author as the category's: a fact `franz` authored in the family
/// wiki becomes `sender = group:famiglia` once `franz` is gone, instead of
/// pointing at a principal that no longer exists.
///
/// Facts are grouped by wiki and each wiki's scope is resolved from topology
/// ([`crate::wiki::WikiTree::resolve_scope_principal`]). A wiki whose scope is
/// *itself* `gone` (the removed principal's own identity wiki) is **skipped** —
/// the substitute would not lift the dangle; those facts belong to the
/// forget-user pass (roadmap 5g). A wiki that fails
/// to locate or resolve is logged and skipped, never aborting the removal. Only
/// active (non-tombstoned) rows are touched. Returns the number reassigned.
///
/// # Errors
///
/// As [`sqlx::Error`] (the distinct-wiki scan or a per-wiki update).
pub async fn reassign_sender_to_scope(
    pool: &SqlitePool,
    tree: &crate::wiki::WikiTree,
    gone: &Principal,
) -> Result<u64> {
    let gone_str = gone.to_string();
    let wikis: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT wiki_id FROM fact_index WHERE sender_id = ? AND deleted_at IS NULL",
    )
    .bind(&gone_str)
    .fetch_all(pool)
    .await?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut reassigned = 0u64;
    for (wiki_id,) in wikis {
        let Ok(id) = crate::types::WikiId::parse(&wiki_id) else {
            tracing::warn!(wiki_id = %wiki_id, "23d: unparsable wiki_id — sender left dangling");
            continue;
        };
        let scope = match tree
            .locate(&id)
            .and_then(|h| tree.resolve_scope_principal(h.meta()))
        {
            Ok(scope) => scope,
            Err(e) => {
                tracing::warn!(
                    wiki_id = %wiki_id, error = %e,
                    "23d: could not resolve wiki scope — sender left dangling"
                );
                continue;
            },
        };
        // The substitute is the removed principal itself (its own identity
        // wiki): reassigning changes nothing, so leave it for forget-user.
        if &scope == gone {
            continue;
        }
        let res = sqlx::query(
            "UPDATE fact_index
                SET sender_id = ?, updated_at = ?
              WHERE wiki_id = ? AND sender_id = ? AND deleted_at IS NULL",
        )
        .bind(scope.to_string())
        .bind(&now)
        .bind(&wiki_id)
        .bind(&gone_str)
        .execute(pool)
        .await?;
        reassigned += res.rows_affected();
    }
    Ok(reassigned)
}

/// Bulk self-delete: tombstone every still-active fact `sender` authored (23d).
///
/// The bulk primitive of the forget model — a contributor clears their own
/// contributions in one act, no vote —
/// optionally narrowed to one wiki (`wiki_id`) and one page (`source_path`).
///
/// The authority is implicit: only facts whose `sender_id` equals the caller's
/// own principal are touched, so this is always a self-delete — the caller can
/// never reach another author's fact through it. `wiki_id = None` means *all
/// wikis*; `source_path = None` means *all pages* (within the wiki, if given).
/// Already-tombstoned / superseded rows are skipped (`deleted_at IS NULL`), so
/// the call is idempotent. Returns the number of rows tombstoned.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_forgotten_by_sender(
    pool: &SqlitePool,
    sender: &Principal,
    wiki_id: Option<&str>,
    source_path: Option<&str>,
    reason: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut sql = String::from(
        "UPDATE fact_index
            SET deleted_at = ?, deleted_reason = ?, updated_at = ?
          WHERE sender_id = ? AND deleted_at IS NULL",
    );
    if wiki_id.is_some() {
        sql.push_str(" AND wiki_id = ?");
    }
    if source_path.is_some() {
        sql.push_str(" AND source_path = ?");
    }
    let mut q = sqlx::query(&sql)
        .bind(&now)
        .bind(reason)
        .bind(&now)
        .bind(sender.to_string());
    if let Some(w) = wiki_id {
        q = q.bind(w.to_owned());
    }
    if let Some(sp) = source_path {
        q = q.bind(sp.to_owned());
    }
    let res = q.execute(pool).await?;
    Ok(res.rows_affected())
}

/// The `fact_id`s [`mark_forgotten_by_sender`] would tombstone.
///
/// The pre-image a bulk self-delete collects (same filter) so it can
/// excise each tombstoned fact's on-disk region afterwards
/// ([`crate::reindex::strip_fact_region`], the disk half of retirement).
///
/// A row that becomes eligible between this snapshot and the bulk UPDATE is
/// missed here (its residue rides the light-dream hygiene sweep); a row that
/// gets retired concurrently is harmless to return — the strip only ever
/// excises retired rows.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn find_active_fact_ids_by_sender(
    pool: &SqlitePool,
    sender: &Principal,
    wiki_id: Option<&str>,
    source_path: Option<&str>,
) -> Result<Vec<FactId>> {
    let mut sql = String::from(
        "SELECT fact_id FROM fact_index
          WHERE sender_id = ? AND deleted_at IS NULL",
    );
    if wiki_id.is_some() {
        sql.push_str(" AND wiki_id = ?");
    }
    if source_path.is_some() {
        sql.push_str(" AND source_path = ?");
    }
    let mut q = sqlx::query_as::<_, (String,)>(&sql).bind(sender.to_string());
    if let Some(w) = wiki_id {
        q = q.bind(w.to_owned());
    }
    if let Some(sp) = source_path {
        q = q.bind(sp.to_owned());
    }
    let rows = q.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id,)| FactId::parse(&id).ok())
        .collect())
}

/// Stamp a successful `wiki_recall` / `wiki_search` hit on every fact
/// id in `hits`, bumping `last_recall_at` and incrementing
/// `recall_count_30d`.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn bump_recall_hits(pool: &SqlitePool, hits: &[FactId]) -> Result<u64> {
    if hits.is_empty() {
        return Ok(0);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut total = 0u64;
    let mut tx = pool.begin().await?;
    for f in hits {
        let r = sqlx::query(
            "UPDATE fact_index
                SET last_recall_at = ?, recall_count_30d = recall_count_30d + 1
              WHERE fact_id = ?",
        )
        .bind(&now)
        .bind(f.as_str())
        .execute(&mut *tx)
        .await?;
        total += r.rows_affected();
    }
    tx.commit().await?;
    Ok(total)
}

/// Delete every fact row pointing at `source_path` (used by the file
/// watcher when an external delete removes the file outright).
///
/// Returns the number of rows removed.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn drop_by_source_path(pool: &SqlitePool, source_path: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM fact_index WHERE source_path = ?")
        .bind(source_path)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Number of non-tombstoned rows whose region lives at `source_path`.
///
/// The path is workdir-relative; superseded rows count — their marker on
/// disk is kept for the revert path. This is the orphan-page sweep's
/// safety check: a page file with ANY live pointer is never deleted.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn count_rows_at_source_path(pool: &SqlitePool, source_path: &str) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fact_index WHERE source_path = ? AND deleted_at IS NULL",
    )
    .bind(source_path)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Number of rows that BLOCK the husk-page GC from removing
/// `source_path`'s file.
///
/// Blocking rows: every **active** row (its region is content — a
/// validity-closed fact still narrates), plus every superseded row
/// whose `superseded_at` is later than `superseded_horizon` (its
/// on-disk marker may still serve a revert — the reason
/// [`count_rows_at_source_path`] keeps the file unconditionally).
/// Tombstoned rows never block, the same posture as the compiler's
/// orphan sweep (the delete-page verb relies on it). Timestamps are
/// RFC 3339 UTC strings, compared lexicographically like the other
/// window queries.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn count_husk_blocking_rows(
    pool: &SqlitePool,
    source_path: &str,
    superseded_horizon: &str,
) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fact_index
          WHERE source_path = ?
            AND deleted_at IS NULL
            AND (superseded_at IS NULL OR superseded_at > ?)",
    )
    .bind(source_path)
    .bind(superseded_horizon)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// The vector dimension of an arbitrary **live** stored embedding.
///
/// `None` when the index has no live rows. The embedder-identity guard
/// ([`crate::reindex::check_embedder_identity`], roadmap 18g) uses it to
/// catch a dimension change even on a store that predates the recorded
/// identity (`engine_meta`).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn sample_embedding_dim(pool: &SqlitePool) -> Result<Option<usize>> {
    let n: Option<i64> =
        sqlx::query_scalar("SELECT embedding_dim FROM fact_index WHERE deleted_at IS NULL LIMIT 1")
            .fetch_optional(pool)
            .await?;
    Ok(n.and_then(|v| usize::try_from(v).ok()))
}

// ---------- Queries ----------

/// Most recent `updated_at` among the **active** facts of one page.
///
/// The page-level freshness signal the recall block's navigated
/// fragments carry in-band. `updated_at` moves only on real mutations
/// (insert, edit, supersede), never on recall-counter bumps, so it is
/// an honest staleness signal. `None` when the page holds no active
/// fact.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn latest_page_activity(
    pool: &SqlitePool,
    wiki_id: &str,
    source_path: &str,
) -> Result<Option<String>> {
    let row: Option<Option<String>> = sqlx::query_scalar(
        "SELECT MAX(updated_at) FROM fact_index
         WHERE wiki_id = ? AND source_path = ?
           AND superseded_at IS NULL AND deleted_at IS NULL",
    )
    .bind(wiki_id)
    .bind(source_path)
    .fetch_optional(pool)
    .await?;
    Ok(row.flatten())
}

/// Authoritative fact-key → ACL map for one page.
///
/// Loads `owner_id` / `allow_ids` / `sender_id` for **every** row whose
/// `source_path` is this file — superseded and tombstoned rows included
/// on purpose: whatever region text still sits in the file is gated by
/// its last-known ACL rather than silently falling back to the page
/// default. The render path resolves each region against this map by
/// fact key first; inline marker attributes are only the fallback for
/// regions the DB does not know
/// (redaction policy).
///
/// The key is `source_path` **alone**, not `(wiki_id, source_path)`: the
/// redaction unit is the **file**, and `source_path` is workdir-relative
/// (`wikis/<id>/<page>`) so it already identifies one physical file
/// globally. A file may legitimately hold regions whose home `wiki_id`
/// differs from the wiki that owns the file — the narrative compiler
/// weaves a related fact from another wiki into a page (a `famiglia` fact
/// cited inside a person's `index.md`). Keying on `wiki_id` too would drop
/// those foreign-home regions from the map, so they fall through to the
/// attribute-less inline marker and redact for **everyone**, including the
/// fact's own owner/sender. `fact_id` is the primary key, so widening the
/// key never collides.
///
/// This full-set map is for **interchange** (export rewrites every on-disk
/// region to its full-marker form, so it needs the ACL of retired regions
/// too). The **reader/redaction** paths use [`page_acl_map_active`] instead,
/// which drops retired rows so a stale marker left on disk redacts
/// fail-closed rather than surfacing to its last-known audience.
///
/// Deliberately skips the embedding/text columns — this runs on every
/// page read.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the principal columns.
pub async fn page_acl_map(pool: &SqlitePool, source_path: &str) -> Result<FactAclMap> {
    page_acl_map_impl(pool, source_path, false).await
}

/// Like [`page_acl_map`] but **excludes superseded and tombstoned rows**.
///
/// A region whose fact the DB has retired (superseded or deleted) but whose
/// bytes still sit on disk is dropped from the map, so the render path finds
/// no ACL for it: a bare runtime marker (`{{f=uuid}}`, no inline attributes)
/// then falls through to the owner-of-last-resort and, having neither an
/// inline owner nor sender, redacts for **everyone** (fail-closed). This is
/// what the reader paths — recall-by-navigation ([`crate::recall_nav`]) and
/// `wiki_read` — must use, so a superseded/contradictory or deleted region
/// is never surfaced from the raw page even before its bytes are stripped.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the principal columns.
pub async fn page_acl_map_active(pool: &SqlitePool, source_path: &str) -> Result<FactAclMap> {
    page_acl_map_impl(pool, source_path, true).await
}

async fn page_acl_map_impl(
    pool: &SqlitePool,
    source_path: &str,
    active_only: bool,
) -> Result<FactAclMap> {
    let sql = if active_only {
        "SELECT fact_id, owner_id, allow_ids, sender_id FROM fact_index
         WHERE source_path = ? AND superseded_at IS NULL AND deleted_at IS NULL"
    } else {
        "SELECT fact_id, owner_id, allow_ids, sender_id FROM fact_index
         WHERE source_path = ?"
    };
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(sql)
        .bind(source_path)
        .fetch_all(pool)
        .await?;

    let mut map = FactAclMap::with_capacity(rows.len());
    for (fact_id, owner, allow, sender) in rows {
        let fact_id = FactId::parse(&fact_id)
            .map_err(|e| sqlx::Error::Decode(format!("fact_id: {e}").into()))?;
        let owner = owner
            .parse::<Principal>()
            .map_err(|e| sqlx::Error::Decode(format!("owner_id: {e}").into()))?;
        let allow = match allow.as_deref() {
            None | Some("") => Vec::new(),
            Some(s) => principals_from_json(s).map_err(|e| sqlx::Error::Decode(e.into()))?,
        };
        let sender = sender
            .map(|s| s.parse::<Principal>())
            .transpose()
            .map_err(|e| sqlx::Error::Decode(format!("sender_id: {e}").into()))?;
        map.insert(
            fact_id,
            RegionAcl {
                owner,
                allow,
                sender,
            },
        );
    }
    Ok(map)
}

/// Lean ACL-and-topics projection of one active fact row, for the recall
/// navigator's **reader-relative card** ([`crate::meta_annotate::build_reader_card`]).
///
/// Carries only what the card boundary needs — the ACL triple plus the
/// fact's `topics` and its containing wiki/page — and deliberately skips
/// the embedding/text columns (like [`page_acl_map`]) so the per-turn card
/// recompute stays cheap.
#[derive(Debug, Clone)]
pub struct CardAclRow {
    /// `wiki_id` of the containing wiki node.
    pub wiki_id: String,
    /// Workdir-relative path of the source `.md` page the fact lives on.
    pub source_path: String,
    /// The fact's **subject** — who/what it is *about* (not its author
    /// `sender_id`, not its audience `allow_ids`).
    pub owner_id: Principal,
    /// `allow=` extension list (possibly empty).
    pub allow_ids: Vec<Principal>,
    /// Cross-user attribution (`None` ⇒ sender equals owner).
    pub sender_id: Option<Principal>,
    /// Free-form topic tags contributed to the reader's visible card.
    pub topics: Vec<String>,
}

/// Every **active** (not superseded, not deleted) fact's ACL-and-topics
/// projection across the whole store, for the reader-relative card recompute.
///
/// One lean query over the full table (no embedding/text decode), grouped and
/// `can_read`-filtered per reader by the caller. Mirrors the column-skipping of
/// [`page_acl_map`]; the navigator runs this per turn (twice — entry-point
/// gather and the funnel), so it stays embedding-free.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the principal / topics JSON columns.
pub async fn active_card_acl_rows(pool: &SqlitePool) -> Result<Vec<CardAclRow>> {
    // (wiki_id, source_path, owner_id, allow_ids, sender_id, topics) as stored.
    type RawCardTuple = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<RawCardTuple> = sqlx::query_as(
        "SELECT wiki_id, source_path, owner_id, allow_ids, sender_id, topics FROM fact_index
             WHERE superseded_at IS NULL AND deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|(wiki_id, source_path, owner, allow, sender, topics)| {
            let owner_id = owner
                .parse::<Principal>()
                .map_err(|e| sqlx::Error::Decode(format!("owner_id: {e}").into()))?;
            let allow_ids = match allow.as_deref() {
                None | Some("") => Vec::new(),
                Some(s) => principals_from_json(s)
                    .map_err(|e| sqlx::Error::Decode(format!("allow_ids: {e}").into()))?,
            };
            let sender_id = sender
                .as_deref()
                .map(str::parse::<Principal>)
                .transpose()
                .map_err(|e| sqlx::Error::Decode(format!("sender_id: {e}").into()))?;
            let topics = topics
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(topics_from_json)
                .transpose()
                .map_err(|e| sqlx::Error::Decode(format!("topics: {e}").into()))?
                .unwrap_or_default();
            Ok(CardAclRow {
                wiki_id,
                source_path,
                owner_id,
                allow_ids,
                sender_id,
                topics,
            })
        })
        .collect()
}

/// Fetch a single row by id, or `None` when no such row exists.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_by_id(pool: &SqlitePool, fact_id: &FactId) -> Result<Option<FactIndexRow>> {
    let raw = sqlx::query_as::<_, RawFactRow>(SELECT_ALL_COLUMNS_WHERE_ID)
        .bind(fact_id.as_str())
        .fetch_optional(pool)
        .await?;
    raw.map(decode_row).transpose()
}

/// Fetch every active (not superseded, not deleted) row in a wiki,
/// ordered by `created_at` ascending. Useful both as a debug helper and
/// as the input set for the dedup check in `wiki_capture`.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_active_in_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<Vec<FactIndexRow>> {
    let rows = sqlx::query_as::<_, RawFactRow>(SELECT_ACTIVE_IN_WIKI)
        .bind(wiki_id)
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(decode_row).collect()
}

/// Whether `wiki_id` **surfaces** to a reader under derived visibility.
///
/// The enforcement of the
/// [identity-and-acl §5](../../../docs/concepts/identity-and-acl.md) rule that a
/// wiki the reader can read nothing in surfaces nowhere (`sender_id` is the
/// reader, `sender_groups` their groups).
///
/// A wiki with **no active facts** surfaces to everyone — there is nothing to
/// hide, and a just-created or not-yet-promoted wiki must not 404 for its own
/// owner. A wiki that **does** hold facts surfaces only to a reader who can read
/// ≥1 of them (the same [`crate::acl::can_read`] the redaction path applies, so
/// a per-fragment `allow=` grant is honoured — it is deliberately *not* gated on
/// the wiki-level `shared_with`). Reads a wiki's *own* rows only
/// (`fact_index.wiki_id`); cheaper than a full reader card for a single verdict.
///
/// # Errors
///
/// The underlying [`find_active_in_wiki`] query error.
pub async fn wiki_visible_to(
    pool: &SqlitePool,
    wiki_id: &str,
    sender_id: &str,
    sender_groups: &[String],
) -> Result<bool> {
    let rows = find_active_in_wiki(pool, wiki_id).await?;
    // Empty wiki: nothing to hide → visible (avoids a 404 on a fresh or
    // buffered-only wiki before the light dream promotes its first fact).
    if rows.is_empty() {
        return Ok(true);
    }
    Ok(rows.iter().any(|row| {
        let acl = crate::types::Acl {
            owner: Some(row.owner_id.clone()),
            allow: row.allow_ids.clone(),
        };
        crate::acl::can_read(&acl, sender_id, sender_groups, row.sender_id.as_ref())
    }))
}

/// Find the wiki's rows **recently closed by a contradiction** — the
/// seeds of the REM contradiction sweep.
///
/// Two shapes qualify, both still readable (never tombstoned): a row a
/// supersede retired (`superseded_at >= since` — the chokepoint stamps
/// its window `contradicted` in the same UPDATE), and a row the closure
/// verb closed as `contradicted` directly (`updated_at >= since`).
/// `since` is ISO 8601, compared via `datetime()` so mixed suffixes
/// normalize.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_recently_contradicted(
    pool: &SqlitePool,
    wiki_id: &str,
    since: &str,
) -> Result<Vec<FactIndexRow>> {
    let sql = r#"
    SELECT fact_id, wiki_id, source_path, region_start, region_end,
           "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
           fact_type, topics, created_at, updated_at, superseded_at,
           superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
           recall_count_30d, valid_from, valid_to, decay_reason,
           target_page, style, page_description, salience, source_ref, authored_refs
      FROM fact_index
     WHERE wiki_id = ?
       AND deleted_at IS NULL
       AND (
            (superseded_at IS NOT NULL AND datetime(superseded_at) >= datetime(?))
         OR (decay_reason = 'contradicted' AND superseded_at IS NULL
             AND datetime(updated_at) >= datetime(?))
       )
     ORDER BY created_at ASC
"#;
    let rows = sqlx::query_as::<_, RawFactRow>(sql)
        .bind(wiki_id)
        .bind(since)
        .bind(since)
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(decode_row).collect()
}

/// Find active rows whose validity window closes (or fires) inside
/// `[from, to]`.
///
/// Both bounds are ISO 8601 UTC strings, compared lexically like every
/// other timestamp column; rows come back ordered by `valid_to` ascending
/// so the most imminent is first. Rows with an OPEN horizon
/// (`valid_to IS NULL`) never match: an open-ended fact has nothing
/// imminent about it.
///
/// This is the storage primitive behind the recall block's **due-soon
/// slot** ([`crate::recall::recall_due_soon`]): a deterministic,
/// time-driven pull — closeness to *now*, not similarity to a query.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_due_between(
    pool: &SqlitePool,
    from: &str,
    to: &str,
    limit: usize,
) -> Result<Vec<FactIndexRow>> {
    let mut sql = String::from(
        r#"SELECT fact_id, wiki_id, source_path, region_start, region_end,
                  "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
                  fact_type, topics, created_at, updated_at, superseded_at,
                  superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
                  recall_count_30d, valid_from, valid_to, decay_reason,
                  target_page, style, page_description, salience, source_ref, authored_refs
             FROM fact_index
            WHERE superseded_at IS NULL AND deleted_at IS NULL
              AND valid_to IS NOT NULL
              AND valid_to >= ? AND valid_to <= ?
            ORDER BY valid_to ASC"#,
    );
    if limit > 0 {
        use std::fmt::Write as _;
        let _ = write!(sql, " LIMIT {limit}");
    }
    let rows = sqlx::query_as::<_, RawFactRow>(&sql)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(decode_row).collect()
}

/// Count how many active rows live in a wiki. Convenience wrapper used
/// by the dashboard's catalog page and by the capture-side dedup early
/// exit (`if 0 rows there is nothing to dedup against`).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn count_active_in_wiki(pool: &SqlitePool, wiki_id: &str) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fact_index
           WHERE wiki_id = ? AND superseded_at IS NULL AND deleted_at IS NULL",
    )
    .bind(wiki_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Structured filter for [`find_by_filters`].
///
/// Every field is optional. `None` means "no constraint on this
/// dimension". When more than one constraint is set they are
/// AND-combined; `topics_any` ANY-matches (the row needs at least one
/// of the listed topics in its `topics` JSON array).
///
/// Tombstone / supersedence are excluded by default — `find_by_filters`
/// is the read-side primitive, so it returns active rows only unless
/// [`FactFilters::include_inactive`] opts in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactFilters {
    /// Scope to a single wiki.
    pub wiki_id: Option<String>,
    /// Scope to an owner principal.
    pub owner_id: Option<Principal>,
    /// Scope to the cross-user attribution principal — the `sender` who
    /// wrote the fact (`fact_index.sender_id`). (Behaviour-rule recall does
    /// NOT ride this: the channel is owner-scoped, via its dedicated
    /// [`find_behaviour_rules`] query.)
    pub sender_id: Option<Principal>,
    /// Scope to a fact-type tag.
    pub fact_type: Option<String>,
    /// Inclusive lower bound on `created_at` (ISO 8601 string compare —
    /// works because we always store ISO 8601 UTC).
    pub created_after: Option<String>,
    /// Exclusive upper bound on `created_at`.
    pub created_before: Option<String>,
    /// ANY-match topics. Empty = no constraint.
    pub topics_any: Vec<String>,
    /// The **dated-query selector**: keep only facts whose validity
    /// window contains this instant (ISO-8601) — `valid_from` absent or
    /// `<=` it, `valid_to` absent (open) or `>` it. This one is a filter
    /// **by design** (an explicitly dated question wants the facts true
    /// at the asked date); the *default* recall path instead treats a
    /// closed window as a down-rank signal
    /// ([`crate::recall::CLOSED_WINDOW_DOWNRANK`]), never a filter.
    /// Bounds compare via `SQLite` `datetime()`, so the mixed `Z` /
    /// `+00:00` suffixes both normalize.
    pub valid_at: Option<String>,
    /// Hard cap on the row count returned (SQL `LIMIT`). 0 = no limit.
    pub limit: usize,
    /// Sort directive. `None` keeps the historical default `created_at DESC`
    /// (so the recall path, which re-ranks by cosine anyway, is unaffected).
    pub sort: Option<FactSort>,
    /// When `true`, **do not** exclude superseded / deleted rows — the
    /// dashboard "includi inattivi" toggle. The default (`false`) keeps the
    /// read-side primitive active-only, as every non-dashboard caller expects.
    pub include_inactive: bool,
}

/// The column a [`FactFilters`] query sorts by.
///
/// A closed whitelist: the SQL `ORDER BY` expression is chosen from
/// [`Self::order_expr`], never interpolated from caller text, so this is
/// injection-safe by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactSortKey {
    /// `created_at` — first-insert wall-clock (the historical default).
    CreatedAt,
    /// `updated_at` — last meaningful update.
    UpdatedAt,
    /// `last_recall_at` — last time the fact surfaced in recall top-K.
    LastRecallAt,
    /// `recall_count_30d` — rolling 30-day recall counter.
    RecallCount30d,
    /// `valid_from` — start of the validity interval.
    ValidFrom,
    /// `valid_to` — end of the validity interval.
    ValidTo,
    /// `fact_type` — taxonomy tag (lexical).
    FactType,
    /// `wiki_id` — containing wiki (lexical).
    WikiId,
    /// `owner_id` — owning principal (lexical).
    OwnerId,
    /// `salience` — ranked by semantic order `high < normal < low`, not lexically.
    Salience,
}

impl FactSortKey {
    /// The SQL `ORDER BY` expression for this key. Fixed strings only.
    const fn order_expr(self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
            Self::LastRecallAt => "last_recall_at",
            Self::RecallCount30d => "recall_count_30d",
            Self::ValidFrom => "valid_from",
            Self::ValidTo => "valid_to",
            Self::FactType => "fact_type",
            Self::WikiId => "wiki_id",
            Self::OwnerId => "owner_id",
            // `high` is most salient → smallest rank, so ASC = high first.
            Self::Salience => {
                "CASE salience WHEN 'high' THEN 0 WHEN 'normal' THEN 1 WHEN 'low' THEN 2 ELSE 3 END"
            },
        }
    }

    /// Parse a dashboard query token (the `sort=` value) into a key.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "created_at" => Some(Self::CreatedAt),
            "updated_at" => Some(Self::UpdatedAt),
            "last_recall_at" => Some(Self::LastRecallAt),
            "recall_count_30d" => Some(Self::RecallCount30d),
            "valid_from" => Some(Self::ValidFrom),
            "valid_to" => Some(Self::ValidTo),
            "fact_type" => Some(Self::FactType),
            "wiki_id" => Some(Self::WikiId),
            "owner_id" => Some(Self::OwnerId),
            "salience" => Some(Self::Salience),
            _ => None,
        }
    }
}

/// A sort directive: which column, and which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactSort {
    /// Column to order by.
    pub key: FactSortKey,
    /// `true` = descending, `false` = ascending.
    pub desc: bool,
}

/// Find active rows matching the given structured filter.
///
/// The query is built dynamically — there's no `WHERE 1=1` pattern;
/// every constraint that's actually set contributes a `AND …` clause.
/// `topics_any` uses `EXISTS (SELECT 1 FROM json_each(topics) WHERE
/// value = ?)` repeated per topic with `OR`, so the `SQLite` JSON1
/// extension is required (it ships enabled in the `sqlx` build we use).
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_by_filters(
    pool: &SqlitePool,
    filters: &FactFilters,
) -> Result<Vec<FactIndexRow>> {
    let mut sql = String::from(
        r#"SELECT fact_id, wiki_id, source_path, region_start, region_end,
                  "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
                  fact_type, topics, created_at, updated_at, superseded_at,
                  superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
                  recall_count_30d, valid_from, valid_to, decay_reason,
                  target_page, style, page_description, salience, source_ref, authored_refs
             FROM fact_index"#,
    );

    // Collect every active constraint as its own predicate, then join — no
    // `WHERE 1=1` anchor. The tombstone exclusion is just another predicate,
    // so `include_inactive` simply omits it.
    let mut preds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if !filters.include_inactive {
        preds.push("superseded_at IS NULL AND deleted_at IS NULL".to_owned());
    }
    if let Some(w) = &filters.wiki_id {
        preds.push("wiki_id = ?".to_owned());
        binds.push(w.clone());
    }
    if let Some(o) = &filters.owner_id {
        preds.push("owner_id = ?".to_owned());
        binds.push(o.to_string());
    }
    if let Some(s) = &filters.sender_id {
        preds.push("sender_id = ?".to_owned());
        binds.push(s.to_string());
    }
    if let Some(t) = &filters.fact_type {
        preds.push("fact_type = ?".to_owned());
        binds.push(t.clone());
    }
    if let Some(after) = &filters.created_after {
        preds.push("created_at >= ?".to_owned());
        binds.push(after.clone());
    }
    if let Some(before) = &filters.created_before {
        preds.push("created_at < ?".to_owned());
        binds.push(before.clone());
    }
    if !filters.topics_any.is_empty() {
        let mut alts: Vec<&str> = Vec::new();
        for topic in &filters.topics_any {
            alts.push(
                "EXISTS (SELECT 1 FROM json_each(fact_index.topics) WHERE json_each.value = ?)",
            );
            binds.push(topic.clone());
        }
        preds.push(format!("({})", alts.join(" OR ")));
    }
    if let Some(at) = &filters.valid_at {
        preds.push(
            "(valid_from IS NULL OR datetime(valid_from) <= datetime(?)) \
             AND (valid_to IS NULL OR datetime(valid_to) > datetime(?))"
                .to_owned(),
        );
        binds.push(at.clone());
        binds.push(at.clone());
    }

    if !preds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&preds.join(" AND "));
    }

    match filters.sort {
        None => sql.push_str(" ORDER BY created_at DESC"),
        Some(s) => {
            sql.push_str(" ORDER BY ");
            sql.push_str(s.key.order_expr());
            sql.push_str(if s.desc { " DESC" } else { " ASC" });
        },
    }
    if filters.limit > 0 {
        sql.push_str(" LIMIT ?");
        binds.push(filters.limit.to_string());
    }

    let mut q = sqlx::query_as::<_, RawFactRow>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await?;
    rows.into_iter().map(decode_row).collect()
}

/// Fetch a principal's active behaviour-rule rows on a wiki's reserved
/// policy page ([`crate::wiki::RULES_FILENAME`]), newest first, capped
/// at `limit` (0 = no cap).
///
/// The storage primitive behind the per-turn behaviour-rules channel
/// (`ingest::recall_behaviour_rules`). Both channel invariants live
/// **in the SQL, before the `LIMIT`**, so the cap counts rules only —
/// unrelated facts sharing the wiki and owner can never starve old
/// rules out of the window:
///
/// - **rules-page predicate** — `source_path LIKE '%/' || 'rules.md'`
///   (every `source_path` is workdir-relative `wikis/<id>/…`, so the
///   file name always follows a `/`). `SQLite` `LIKE` is ASCII-case-
///   insensitive, so the decoded rows are re-checked against the exact
///   [`crate::wiki::is_rules_page`] predicate.
/// - **validity filter** — a rule whose window is closed at `valid_at`
///   (`valid_to` set and past) is NOT served: a retracted rule must
///   stop steering the agent. Same window-contains-instant shape as
///   [`FactFilters::valid_at`]. For ordinary facts a closed window is
///   a recall *down-rank signal*, never a filter — the rules channel
///   is the deliberate exception.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_behaviour_rules(
    pool: &SqlitePool,
    wiki_id: &str,
    owner: &Principal,
    valid_at: &str,
    limit: usize,
) -> Result<Vec<FactIndexRow>> {
    let mut sql = String::from(
        r#"SELECT fact_id, wiki_id, source_path, region_start, region_end,
                  "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
                  fact_type, topics, created_at, updated_at, superseded_at,
                  superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
                  recall_count_30d, valid_from, valid_to, decay_reason,
                  target_page, style, page_description, salience, source_ref, authored_refs
             FROM fact_index
            WHERE wiki_id = ?
              AND owner_id = ?
              AND superseded_at IS NULL AND deleted_at IS NULL
              AND source_path LIKE ?
              AND (valid_from IS NULL OR datetime(valid_from) <= datetime(?))
              AND (valid_to IS NULL OR datetime(valid_to) > datetime(?))
            ORDER BY created_at DESC"#,
    );
    if limit > 0 {
        use std::fmt::Write as _;
        let _ = write!(sql, " LIMIT {limit}");
    }
    let rows = sqlx::query_as::<_, RawFactRow>(&sql)
        .bind(wiki_id)
        .bind(owner.to_string())
        .bind(format!("%/{}", crate::wiki::RULES_FILENAME))
        .bind(valid_at)
        .bind(valid_at)
        .fetch_all(pool)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for raw in rows {
        let row = decode_row(raw)?;
        // Exactness re-check on the case-insensitive LIKE pre-filter: the
        // channel serves only what the rest of the engine treats as the
        // rules page (`is_rules_page` is byte-exact on the file name).
        if crate::wiki::is_rules_page(&row.source_path) {
            out.push(row);
        }
    }
    Ok(out)
}

/// Return the set of source paths the index knows about under the
/// given wiki. Used by re-index housekeeping ("which files do I
/// already cover").
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn distinct_source_paths(pool: &SqlitePool, wiki_id: &str) -> Result<HashSet<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT source_path FROM fact_index WHERE wiki_id = ?")
            .bind(wiki_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Fetch every active (not superseded, not deleted) row whose
/// `source_path` matches the given value, ordered by `created_at` ASC.
///
/// Used by the [`crate::reindex`] pipeline to diff a parsed file against
/// the DB before applying inserts / updates / orphan tombstones.
///
/// # Errors
///
/// `sqlx::Error` + decode errors on the embedding / JSON columns.
pub async fn find_active_by_source_path(
    pool: &SqlitePool,
    source_path: &str,
) -> Result<Vec<FactIndexRow>> {
    let rows = sqlx::query_as::<_, RawFactRow>(SELECT_ACTIVE_BY_SOURCE_PATH)
        .bind(source_path)
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(decode_row).collect()
}

/// Region payload required by [`update_region`].
///
/// Mirrors the subset of [`NewFact`] that may be mutated in place when
/// an existing region's body / offsets changed on disk. **Deliberately
/// carries no ACL fields**: the ACL columns are the authoritative
/// source (redaction policy)
/// and are never rewritten from what a file says — only a dedicated
/// ACL-edit operation may touch them. `created_at` and the recall
/// counters are preserved.
#[derive(Debug, Clone)]
pub struct RegionUpdate {
    /// New byte offset of the opening `{{f=…}}` marker.
    pub region_start: Option<i64>,
    /// New byte offset one past the closing `{{/}}` marker.
    pub region_end: Option<i64>,
    /// New region body verbatim (no markers).
    pub text: String,
    /// New embedding for the body.
    pub embedding: Vec<f32>,
}

/// Update an existing active row in place. Sets the new region offsets,
/// body text, and embedding, and bumps `updated_at`. The ACL columns
/// are untouched by design (see [`RegionUpdate`]).
///
/// Returns the number of rows touched (0 when `fact_id` is unknown,
/// superseded, or tombstoned — the caller can treat that as a no-op).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn update_region(
    pool: &SqlitePool,
    fact_id: &FactId,
    update: &RegionUpdate,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let embedding_dim = i64::try_from(update.embedding.len()).unwrap_or(i64::MAX);
    let blob = encode_embedding(&update.embedding);
    let res = sqlx::query(
        r#"UPDATE fact_index
              SET region_start = ?, region_end = ?, "text" = ?,
                  embedding = ?, embedding_dim = ?,
                  updated_at = ?
            WHERE fact_id = ?
              AND superseded_at IS NULL
              AND deleted_at IS NULL"#,
    )
    .bind(update.region_start)
    .bind(update.region_end)
    .bind(&update.text)
    .bind(&blob)
    .bind(embedding_dim)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// [`update_region`] variant that also replaces the row's `authored_refs`
/// in the **same** `UPDATE` statement.
///
/// Used by the REM provenance-hygiene sweep, which moves a trailing
/// source-pointer wikilink out of the canonical text and into
/// `authored_refs`: one atomic statement means a crash can never leave the
/// pointer stripped from `text` but not yet recorded in `authored_refs`.
/// ACL columns stay untouched, exactly like [`update_region`].
///
/// # Errors
///
/// As [`sqlx::Error`]; [`FactIndexError::Json`] if `authored_refs` fails
/// JSON encoding.
pub async fn update_region_and_authored_refs(
    pool: &SqlitePool,
    fact_id: &FactId,
    update: &RegionUpdate,
    authored_refs: &[String],
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let embedding_dim = i64::try_from(update.embedding.len()).unwrap_or(i64::MAX);
    let blob = encode_embedding(&update.embedding);
    let refs_json = topics_to_json(authored_refs)?;
    let res = sqlx::query(
        r#"UPDATE fact_index
              SET region_start = ?, region_end = ?, "text" = ?,
                  embedding = ?, embedding_dim = ?,
                  authored_refs = ?,
                  updated_at = ?
            WHERE fact_id = ?
              AND superseded_at IS NULL
              AND deleted_at IS NULL"#,
    )
    .bind(update.region_start)
    .bind(update.region_end)
    .bind(&update.text)
    .bind(&blob)
    .bind(embedding_dim)
    .bind(&refs_json)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Clear the supersede chain on a row, but only if it is still
/// superseded by `expected_superseded_by`. Inverse of [`mark_superseded`]
/// used by the `dedup_merge` structure-proposal revert path.
///
/// The conditional `WHERE superseded_by = ?` is load-bearing: if a
/// later supersede has overwritten the chain (`old → new` later became
/// `old → new → newer`), this clear is a no-op and the handler surfaces
/// a clean error rather than orphaning `newer`.
///
/// `superseded_at` is reset to `NULL` and `updated_at` is bumped.
/// Returns the number of rows touched (0 when the chain has moved on
/// or the row was tombstoned).
///
/// # Errors
///
/// `sqlx::Error` only.
pub async fn clear_supersede(
    pool: &SqlitePool,
    fact_id: &FactId,
    expected_superseded_by: &FactId,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        r"UPDATE fact_index
             SET superseded_at = NULL,
                 superseded_by = NULL,
                 updated_at = ?
           WHERE fact_id = ?
             AND superseded_by = ?
             AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(fact_id.as_str())
    .bind(expected_superseded_by.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Move an active row to a new `source_path` with fresh region offsets.
///
/// Unlike [`update_region`] which keeps the row at the same path and
/// rewrites body/embedding/ACL, this primitive flips `source_path` and
/// the byte offsets only — body, embedding, ACL, `created_at`, recall
/// counters, attribution all stay untouched. Used by the structure
/// proposal `wiki_promote` handler when facts are relocated across
/// pages.
///
/// Returns the number of rows touched (0 when `fact_id` is unknown,
/// already superseded, or tombstoned).
///
/// # Errors
///
/// `sqlx::Error` only.
pub async fn move_region(
    pool: &SqlitePool,
    fact_id: &FactId,
    new_source_path: &str,
    new_region_start: Option<i64>,
    new_region_end: Option<i64>,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        r"UPDATE fact_index
              SET source_path = ?,
                  region_start = ?,
                  region_end = ?,
                  updated_at = ?
            WHERE fact_id = ?
              AND superseded_at IS NULL
              AND deleted_at IS NULL",
    )
    .bind(new_source_path)
    .bind(new_region_start)
    .bind(new_region_end)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Move an active row to a **different wiki** in one atomic statement.
///
/// Flips `wiki_id`, `source_path` and the byte offsets together. Body,
/// embedding, ACL, `created_at`, recall counters and attribution all
/// stay untouched — the fact keeps its identity (and its per-fragment
/// ACL) across the wiki boundary.
///
/// Unlike [`move_region`] (same wiki, `source_path` + offsets only) this
/// is the only primitive that repoints `wiki_id`. Used by the
/// `wiki_promote` cross-wiki variants (`file_to_subwiki`, `fact_refile`)
/// where a fact relocates from one wiki to another. Pass `None` for the
/// offsets to land the row as a "pending render" (the orphan sweep
/// spares NULL-offset rows) during the load-bearing DB-first commit
/// order, then call again with the rendered offsets.
///
/// Returns the number of rows touched (0 when `fact_id` is unknown,
/// already superseded, or tombstoned).
///
/// # Errors
///
/// `sqlx::Error` only.
pub async fn move_to_wiki(
    pool: &SqlitePool,
    fact_id: &FactId,
    new_wiki_id: &str,
    new_source_path: &str,
    new_region_start: Option<i64>,
    new_region_end: Option<i64>,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        r"UPDATE fact_index
              SET wiki_id = ?,
                  source_path = ?,
                  region_start = ?,
                  region_end = ?,
                  updated_at = ?
            WHERE fact_id = ?
              AND superseded_at IS NULL
              AND deleted_at IS NULL",
    )
    .bind(new_wiki_id)
    .bind(new_source_path)
    .bind(new_region_start)
    .bind(new_region_end)
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Rebase every `fact_index.source_path` that starts with `old_prefix`
/// onto `new_prefix`, preserving the suffix after the prefix.
///
/// Used by [`crate::scope::wiki_change_scope`] to keep the
/// fact-index `source_path` column consistent with the filesystem
/// after a directory rename: the on-disk content (and therefore the
/// region offsets) is identical, only the path prefix shifted.
///
/// Both prefixes are POSIX-style relative paths matching the canonical
/// `source_path` form produced by
/// [`crate::wiki::workdir_relative_source_path`] — callers are
/// expected to include the trailing `/` so the prefix never matches a
/// sibling whose name happens to share the source's leading bytes
/// (e.g. `wikis/alice/` vs `wikis/alice-bis/`).
///
/// Returns the number of rows touched. The function does not bump
/// `updated_at` — paths-only metadata changes are not user-meaningful.
///
/// # Errors
///
/// `sqlx::Error` only.
pub async fn rebase_source_path_prefix(
    pool: &SqlitePool,
    old_prefix: &str,
    new_prefix: &str,
) -> Result<u64> {
    if old_prefix == new_prefix {
        return Ok(0);
    }
    let like_pattern = format!("{old_prefix}%");
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT fact_id, source_path FROM fact_index WHERE source_path LIKE ?")
            .bind(&like_pattern)
            .fetch_all(pool)
            .await?;
    let mut touched = 0u64;
    for (fact_id, source_path) in rows {
        let Some(suffix) = source_path.strip_prefix(old_prefix) else {
            continue;
        };
        let new_path = format!("{new_prefix}{suffix}");
        let res = sqlx::query("UPDATE fact_index SET source_path = ? WHERE fact_id = ?")
            .bind(&new_path)
            .bind(&fact_id)
            .execute(pool)
            .await?;
        touched += res.rows_affected();
    }
    Ok(touched)
}

// ---------- Retirement disk-half helpers ----------
//
// The DB tombstone (`superseded_at` / `deleted_at`) is the authoritative
// half of retirement; these helpers back the *disk* half
// ([`crate::reindex::strip_fact_region`] and the light-dream hygiene
// sweep), which excises the retired region's bytes from its page and then
// settles the row's now-meaningless offsets to NULL so the sweep's
// candidate query converges.

/// Clear the region offsets of one **retired** row.
///
/// Guarded on retirement (superseded or tombstoned) so it can never turn
/// a live row into a false pending render; the exact complement of
/// [`move_region`], which touches only active rows.
///
/// Called after the row's on-disk region was excised (or verified absent):
/// NULL offsets record "no rendered bytes". If the row is later revived
/// (a dedup-merge revert, a bundle restore) it comes back as a clean
/// pending render the next compile re-renders.
///
/// Returns the number of rows touched (0 when unknown or still active).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn clear_region_offsets_retired(pool: &SqlitePool, fact_id: &FactId) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET region_start = NULL, region_end = NULL, updated_at = ?
          WHERE fact_id = ?
            AND (superseded_at IS NOT NULL OR deleted_at IS NOT NULL)",
    )
    .bind(&now)
    .bind(fact_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bulk variant of [`clear_region_offsets_retired`] for a page that no
/// longer exists on disk: every retired row still pointing at
/// `source_path` drops its offsets in one statement (no bytes can remain).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn clear_region_offsets_retired_on_page(
    pool: &SqlitePool,
    source_path: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index
            SET region_start = NULL, region_end = NULL, updated_at = ?
          WHERE source_path = ?
            AND (superseded_at IS NOT NULL OR deleted_at IS NOT NULL)
            AND region_start IS NOT NULL",
    )
    .bind(&now)
    .bind(source_path)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Distinct `source_path`s that may still carry retired-region residue.
///
/// Pages referenced by a retired row whose offsets were never settled —
/// the candidate list of the light-dream hygiene sweep. Once a page's
/// residue is stripped (or verified gone) the rows' offsets are cleared
/// and the page drops out, so the sweep converges.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn retired_region_pages(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT source_path FROM fact_index
          WHERE (superseded_at IS NOT NULL OR deleted_at IS NOT NULL)
            AND region_start IS NOT NULL
          ORDER BY source_path ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// The retired rows still holding offsets on one page.
///
/// The set the page-level strip settles after excising (or failing to
/// find) their markers. Returned as raw id strings so a malformed legacy
/// id cannot abort the hygiene pass.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn retired_region_fact_ids_on_page(
    pool: &SqlitePool,
    source_path: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT fact_id FROM fact_index
          WHERE source_path = ?
            AND (superseded_at IS NOT NULL OR deleted_at IS NOT NULL)
            AND region_start IS NOT NULL
          ORDER BY fact_id ASC",
    )
    .bind(source_path)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// ---------- wiki_id ↔ source_path reconciliation (boot safety net) ----------

/// Slim location projection of one active row.
///
/// The unit of the boot-time `wiki_id` reconcile pass
/// ([`crate::reindex::reconcile_wiki_ids`]). Raw strings on purpose — a
/// repair pass must not choke on a malformed legacy id, and it never
/// needs the embedding/text payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactLocation {
    /// Raw `fact_id` column value.
    pub fact_id: String,
    /// The wiki the row claims to belong to.
    pub wiki_id: String,
    /// Workdir-relative path of the row's page.
    pub source_path: String,
}

/// Every active row's `(fact_id, wiki_id, source_path)` triple, ordered by
/// `fact_id` for determinism. Deliberately skips the embedding/text columns
/// — this runs once per boot over the whole table.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn all_active_locations(pool: &SqlitePool) -> Result<Vec<FactLocation>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT fact_id, wiki_id, source_path FROM fact_index
          WHERE superseded_at IS NULL AND deleted_at IS NULL
          ORDER BY fact_id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(fact_id, wiki_id, source_path)| FactLocation {
            fact_id,
            wiki_id,
            source_path,
        })
        .collect())
}

/// Targeted `wiki_id` repair for one row.
///
/// Repoints the row from `expected_wiki_id` to `new_wiki_id`, leaving
/// `source_path` and the offsets untouched (the path already tells the
/// truth — the wiki column is what diverged).
///
/// Guarded on the current value so a concurrent legitimate move
/// ([`move_to_wiki`]) makes this a 0-row no-op instead of clobbering it;
/// idempotent by the same guard. Takes the raw id string because the boot
/// reconcile pass iterates raw [`FactLocation`]s.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn set_wiki_id(
    pool: &SqlitePool,
    fact_id: &str,
    expected_wiki_id: &str,
    new_wiki_id: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE fact_index SET wiki_id = ?, updated_at = ?
          WHERE fact_id = ? AND wiki_id = ?",
    )
    .bind(new_wiki_id)
    .bind(&now)
    .bind(fact_id)
    .bind(expected_wiki_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ---------- Raw row + decode adapter ----------

const SELECT_ALL_COLUMNS_WHERE_ID: &str = r#"
    SELECT fact_id, wiki_id, source_path, region_start, region_end,
           "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
           fact_type, topics, created_at, updated_at, superseded_at,
           superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
           recall_count_30d, valid_from, valid_to, decay_reason,
           target_page, style, page_description, salience, source_ref, authored_refs
      FROM fact_index
     WHERE fact_id = ?
"#;

const SELECT_ACTIVE_IN_WIKI: &str = r#"
    SELECT fact_id, wiki_id, source_path, region_start, region_end,
           "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
           fact_type, topics, created_at, updated_at, superseded_at,
           superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
           recall_count_30d, valid_from, valid_to, decay_reason,
           target_page, style, page_description, salience, source_ref, authored_refs
      FROM fact_index
     WHERE wiki_id = ?
       AND superseded_at IS NULL
       AND deleted_at IS NULL
     ORDER BY created_at ASC
"#;

const SELECT_ACTIVE_BY_SOURCE_PATH: &str = r#"
    SELECT fact_id, wiki_id, source_path, region_start, region_end,
           "text", embedding, embedding_dim, owner_id, allow_ids, sender_id,
           fact_type, topics, created_at, updated_at, superseded_at,
           superseded_by, successor_fact_id, deleted_at, deleted_reason, last_recall_at,
           recall_count_30d, valid_from, valid_to, decay_reason,
           target_page, style, page_description, salience, source_ref, authored_refs
      FROM fact_index
     WHERE source_path = ?
       AND superseded_at IS NULL
       AND deleted_at IS NULL
     ORDER BY created_at ASC
"#;

#[derive(sqlx::FromRow)]
struct RawFactRow {
    fact_id: String,
    wiki_id: String,
    source_path: String,
    region_start: Option<i64>,
    region_end: Option<i64>,
    text: String,
    embedding: Vec<u8>,
    #[allow(dead_code)]
    embedding_dim: i64,
    owner_id: String,
    allow_ids: Option<String>,
    sender_id: Option<String>,
    fact_type: Option<String>,
    topics: Option<String>,
    created_at: String,
    updated_at: String,
    superseded_at: Option<String>,
    superseded_by: Option<String>,
    successor_fact_id: Option<String>,
    deleted_at: Option<String>,
    deleted_reason: Option<String>,
    last_recall_at: Option<String>,
    recall_count_30d: i64,
    valid_from: Option<String>,
    valid_to: Option<String>,
    decay_reason: Option<String>,
    target_page: Option<String>,
    style: Option<String>,
    page_description: Option<String>,
    salience: Option<String>,
    source_ref: Option<String>,
    authored_refs: Option<String>,
}

fn decode_row(raw: RawFactRow) -> Result<FactIndexRow> {
    let fact_id = FactId::parse(&raw.fact_id)
        .map_err(|e| sqlx::Error::Decode(format!("fact_id: {e}").into()))?;
    let owner = raw
        .owner_id
        .parse::<Principal>()
        .map_err(|e| sqlx::Error::Decode(format!("owner_id: {e}").into()))?;
    let allow_ids = match raw.allow_ids.as_deref() {
        None | Some("") => Vec::new(),
        Some(s) => principals_from_json(s)
            .map_err(|e| sqlx::Error::Decode(format!("allow_ids: {e}").into()))?,
    };
    let sender_id = raw
        .sender_id
        .as_deref()
        .map(str::parse::<Principal>)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(format!("sender_id: {e}").into()))?;
    let topics = raw
        .topics
        .as_deref()
        .map(topics_from_json)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(format!("topics: {e}").into()))?
        .unwrap_or_default();
    let authored_refs = raw
        .authored_refs
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(topics_from_json)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(format!("authored_refs: {e}").into()))?
        .unwrap_or_default();
    let superseded_by = raw
        .superseded_by
        .as_deref()
        .map(FactId::parse)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(format!("superseded_by: {e}").into()))?;
    let successor_fact_id = raw
        .successor_fact_id
        .as_deref()
        .map(FactId::parse)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(format!("successor_fact_id: {e}").into()))?;
    let embedding = decode_embedding(&raw.embedding)?;

    Ok(FactIndexRow {
        fact_id,
        wiki_id: raw.wiki_id,
        source_path: raw.source_path,
        region_start: raw.region_start,
        region_end: raw.region_end,
        text: raw.text,
        embedding,
        owner_id: owner,
        allow_ids,
        sender_id,
        fact_type: raw.fact_type,
        topics,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        superseded_at: raw.superseded_at,
        superseded_by,
        successor_fact_id,
        deleted_at: raw.deleted_at,
        deleted_reason: raw.deleted_reason,
        last_recall_at: raw.last_recall_at,
        recall_count_30d: raw.recall_count_30d,
        valid_from: raw.valid_from,
        valid_to: raw.valid_to,
        decay_reason: raw.decay_reason,
        salience: raw.salience,
        target_page: raw.target_page,
        style: raw.style,
        page_description: raw.page_description,
        source_ref: raw.source_ref,
        authored_refs,
    })
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

    const SAMPLE_UUID_V7_1: &str = "018f1234-5678-7abc-9def-0123456789ab";
    const SAMPLE_UUID_V7_2: &str = "018f1234-5678-7abc-9def-0123456789ac";
    const SAMPLE_UUID_V7_3: &str = "018f1234-5678-7abc-9def-0123456789ad";
    const SAMPLE_UUID_V7_4: &str = "018f1234-5678-7abc-9def-0123456789ae";

    fn sample_new_fact(fact_id_str: &str, wiki: &str, owner: &str, text: &str) -> NewFact {
        NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(fact_id_str).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: format!("wikis/{wiki}/intro.md"),
            region_start: Some(0),
            region_end: Some(64),
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            owner_id: owner.parse().unwrap(),
            allow_ids: vec!["group:family".parse().unwrap()],
            sender_id: Some("user:bob".parse().unwrap()),
            fact_type: Some("preference".to_owned()),
            topics: vec!["food".to_owned(), "italian".to_owned()],
            valid_from: None,
            valid_to: None,
            // Re-derived/non-ingest fact — no
            // classifier placement proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        }
    }

    // ---------- identity core ----------

    #[tokio::test]
    async fn is_identity_core_true_only_for_bio_and_high() {
        let pool = make_pool().await;
        // bio + high → identity core (a role / relationship).
        let mut core = sample_new_fact(
            SAMPLE_UUID_V7_1,
            "franz",
            "user:franz",
            "Frodo è il compagno di Galadriel",
        );
        core.fact_type = Some("bio".to_owned());
        core.salience = Some("high".to_owned());
        insert_if_absent(&pool, &core).await.unwrap();
        // bio but normal salience (a bio trivium) → NOT core.
        let mut trivium = sample_new_fact(
            SAMPLE_UUID_V7_2,
            "franz",
            "user:franz",
            "Il secondo nome di Franz è Carlo",
        );
        trivium.fact_type = Some("bio".to_owned());
        trivium.salience = Some("normal".to_owned());
        insert_if_absent(&pool, &trivium).await.unwrap();
        // high salience but not bio (a health state) → NOT core.
        let mut health = sample_new_fact(SAMPLE_UUID_V7_3, "franz", "user:franz", "È celiaco");
        health.fact_type = Some("state".to_owned());
        health.salience = Some("high".to_owned());
        insert_if_absent(&pool, &health).await.unwrap();

        let get = |id: &str| {
            let pool = pool.clone();
            let id = FactId::parse(id).unwrap();
            async move { find_by_id(&pool, &id).await.unwrap().unwrap() }
        };
        assert!(
            get(SAMPLE_UUID_V7_1).await.is_identity_core(),
            "bio + high is the identity core"
        );
        assert!(
            !get(SAMPLE_UUID_V7_2).await.is_identity_core(),
            "bio + normal is not core (a trivium)"
        );
        assert!(
            !get(SAMPLE_UUID_V7_3).await.is_identity_core(),
            "high but not bio is not core (health)"
        );
    }

    // ---------- encode/decode embedding ----------

    #[test]
    fn embedding_encode_decode_roundtrip() {
        let v = vec![1.0_f32, -0.5, 0.0, 3.5, -42.0];
        let bytes = encode_embedding(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        let back = decode_embedding(&bytes).expect("decode");
        assert_eq!(back, v);
    }

    #[test]
    fn embedding_decode_rejects_invalid_length() {
        let bytes = vec![0u8, 0, 0]; // 3 bytes, not divisible by 4.
        let err = decode_embedding(&bytes).expect_err("must reject");
        assert!(matches!(err, FactIndexError::InvalidEmbeddingBlob(3)));
    }

    // ---------- principals JSON ----------

    #[test]
    fn principals_json_roundtrip() {
        let ps = vec![
            "user:alice".parse::<Principal>().unwrap(),
            "group:family".parse().unwrap(),
            "global".parse().unwrap(),
        ];
        let json = principals_to_json(&ps).expect("encode");
        let back = principals_from_json(&json).expect("decode");
        assert_eq!(back, ps);
    }

    // ---------- 23d: deleted-principal sender reassignment ----------

    #[tokio::test]
    async fn reassign_sender_to_scope_substitutes_gone_author() {
        use crate::wiki::WikiTree;
        let pool = make_pool().await;

        // A group wiki `famiglia` on disk → scope principal group:famiglia.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis/famiglia")).unwrap();
        std::fs::write(
            dir.path().join("wikis/famiglia/_meta.md"),
            "---\nwiki_id: famiglia\nwiki_type: wiki-group\nparent_wiki_id: null\n\
             slug: famiglia\ntitle: famiglia\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        // A fact franz authored in the family wiki (owner = the collective).
        let mut f = sample_new_fact(
            SAMPLE_UUID_V7_1,
            "famiglia",
            "group:famiglia",
            "we go to the sea in July",
        );
        f.sender_id = Some("user:franz".parse().unwrap());
        f.allow_ids = vec!["group:famiglia".parse().unwrap()];
        insert(&pool, &f).await.expect("insert");

        // franz is removed → his sender is reassigned to the wiki scope.
        let gone = "user:franz".parse::<Principal>().unwrap();
        let n = reassign_sender_to_scope(&pool, &tree, &gone)
            .await
            .expect("reassign");
        assert_eq!(n, 1, "one fact reassigned");
        let back = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(
            back.sender_id,
            Some("group:famiglia".parse().unwrap()),
            "sender now the wiki scope, not the vanished franz"
        );

        // Idempotent: a second pass finds nothing still attributed to franz.
        let n2 = reassign_sender_to_scope(&pool, &tree, &gone)
            .await
            .expect("reassign2");
        assert_eq!(n2, 0, "nothing left to reassign");
    }

    #[tokio::test]
    async fn reassign_sender_skips_when_scope_is_the_gone_principal() {
        use crate::wiki::WikiTree;
        let pool = make_pool().await;

        // franz's own identity wiki → scope principal user:franz.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis/franz")).unwrap();
        std::fs::write(
            dir.path().join("wikis/franz/_meta.md"),
            "---\nwiki_id: franz\nwiki_type: wiki-user\nparent_wiki_id: null\n\
             slug: franz\ntitle: franz\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();

        let mut f = sample_new_fact(SAMPLE_UUID_V7_2, "franz", "user:franz", "I like rust");
        f.sender_id = Some("user:franz".parse().unwrap());
        f.allow_ids = vec![];
        insert(&pool, &f).await.expect("insert");

        // Reassigning would substitute user:franz with user:franz — a no-op the
        // helper skips, leaving the fact for the forget-user pass.
        let gone = "user:franz".parse::<Principal>().unwrap();
        let n = reassign_sender_to_scope(&pool, &tree, &gone)
            .await
            .expect("reassign");
        assert_eq!(n, 0, "scope == gone is skipped");
        let back = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(back.sender_id, Some("user:franz".parse().unwrap()));
    }

    // ---------- bulk self-delete ----------

    #[tokio::test]
    async fn mark_forgotten_by_sender_is_scoped_self_delete() {
        let pool = make_pool().await;
        // franz authored one fact in alice and one in salute; bob authored one
        // in alice (same page as franz's). All source_paths are wikis/<w>/intro.md.
        let mut a = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "franz in alice");
        a.sender_id = Some("user:franz".parse().unwrap());
        insert(&pool, &a).await.unwrap();
        let mut b = sample_new_fact(SAMPLE_UUID_V7_2, "salute", "user:alice", "franz in salute");
        b.sender_id = Some("user:franz".parse().unwrap());
        insert(&pool, &b).await.unwrap();
        let mut c = sample_new_fact(SAMPLE_UUID_V7_3, "alice", "user:alice", "bob in alice");
        c.sender_id = Some("user:bob".parse().unwrap());
        insert(&pool, &c).await.unwrap();

        let franz = "user:franz".parse::<Principal>().unwrap();
        let deleted = |row: FactIndexRow| row.deleted_at.is_some();

        // Page scope: only franz's fact on alice/intro.md — not bob's (different
        // sender), not franz's salute fact (different page).
        let n = mark_forgotten_by_sender(
            &pool,
            &franz,
            Some("alice"),
            Some("wikis/alice/intro.md"),
            "bulk",
        )
        .await
        .unwrap();
        assert_eq!(n, 1, "exactly franz's alice fact");
        assert!(deleted(
            find_by_id(&pool, &a.fact_id).await.unwrap().unwrap()
        ));
        assert!(
            !deleted(find_by_id(&pool, &c.fact_id).await.unwrap().unwrap()),
            "bob's fact on the same page survives"
        );
        assert!(
            !deleted(find_by_id(&pool, &b.fact_id).await.unwrap().unwrap()),
            "franz's salute fact (other page) survives"
        );

        // All scope: the remaining franz fact (salute) goes; bob's stays.
        let n = mark_forgotten_by_sender(&pool, &franz, None, None, "bulk")
            .await
            .unwrap();
        assert_eq!(n, 1, "franz's last fact");
        assert!(deleted(
            find_by_id(&pool, &b.fact_id).await.unwrap().unwrap()
        ));
        assert!(!deleted(
            find_by_id(&pool, &c.fact_id).await.unwrap().unwrap()
        ));
    }

    // ---------- insert + find ----------

    #[tokio::test]
    async fn insert_then_find_by_id() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "I love pasta");
        insert(&pool, &f).await.expect("insert");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(back.fact_id, f.fact_id);
        assert_eq!(back.wiki_id, "alice");
        assert_eq!(back.text, "I love pasta");
        assert_eq!(back.owner_id, "user:alice".parse().unwrap());
        assert_eq!(back.allow_ids, vec!["group:family".parse().unwrap()]);
        assert_eq!(back.sender_id, Some("user:bob".parse().unwrap()));
        assert_eq!(back.topics, vec!["food".to_owned(), "italian".to_owned()]);
        assert_eq!(back.embedding, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(back.superseded_at.is_none());
        assert!(back.deleted_at.is_none());
    }

    #[tokio::test]
    async fn find_by_filters_sort_and_include_inactive() {
        let pool = make_pool().await;
        // Two active facts (a, b) and one we will supersede (c).
        for (id, text) in [
            (SAMPLE_UUID_V7_1, "alpha"),
            (SAMPLE_UUID_V7_2, "bravo"),
            (SAMPLE_UUID_V7_3, "charlie"),
        ] {
            insert(&pool, &sample_new_fact(id, "alice", "user:alice", text))
                .await
                .expect("insert");
        }
        let c_id = FactId::parse(SAMPLE_UUID_V7_3).unwrap();
        let replacement = FactId::parse(SAMPLE_UUID_V7_1).unwrap();
        mark_superseded(&pool, &c_id, &replacement)
            .await
            .expect("supersede");

        // Default (active-only) excludes the superseded row.
        let active = find_by_filters(&pool, &FactFilters::default())
            .await
            .expect("active");
        assert_eq!(active.len(), 2, "superseded row excluded by default");

        // include_inactive surfaces it.
        let all = find_by_filters(
            &pool,
            &FactFilters {
                include_inactive: true,
                ..Default::default()
            },
        )
        .await
        .expect("all");
        assert_eq!(all.len(), 3, "include_inactive surfaces the superseded row");

        // Sorting by salience exercises the `CASE … END` ORDER BY branch — the
        // one expression that must run without a SQL error and still return
        // the active set.
        let sorted = find_by_filters(
            &pool,
            &FactFilters {
                sort: Some(FactSort {
                    key: FactSortKey::Salience,
                    desc: false,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("sorted");
        assert_eq!(sorted.len(), 2);
    }

    /// The behaviour-rules channel query: the rules-page predicate and the
    /// validity filter live IN the SQL, before the `LIMIT` — so non-rules
    /// facts under the same owner never consume the cap (the starvation
    /// regression), a closed-window rule is not served, and other owners'
    /// rules stay out.
    #[tokio::test]
    async fn find_behaviour_rules_filters_page_validity_and_owner_before_the_cap() {
        let pool = make_pool().await;
        let at = "2026-07-02T12:00:00Z";

        // Open rule (oldest), closed rule, other-owner rule, and a NEWEST
        // non-rules crowder under the same owner.
        let mut open_rule = sample_new_fact(SAMPLE_UUID_V7_1, "agent", "user:bob", "Dammi del tu.");
        open_rule.source_path = "wikis/agent/rules.md".to_owned();
        let mut closed_rule =
            sample_new_fact(SAMPLE_UUID_V7_2, "agent", "user:bob", "Chiamami Sam.");
        closed_rule.source_path = "wikis/agent/rules.md".to_owned();
        closed_rule.valid_to = Some("2026-07-01T00:00:00Z".to_owned()); // past `at`
        let mut foreign_rule =
            sample_new_fact(SAMPLE_UUID_V7_3, "agent", "user:alice", "Dai del lei.");
        foreign_rule.source_path = "wikis/agent/rules.md".to_owned();
        let crowder = sample_new_fact(SAMPLE_UUID_V7_4, "agent", "user:bob", "self-fact");
        for f in [&open_rule, &closed_rule, &foreign_rule, &crowder] {
            insert(&pool, f).await.expect("insert");
        }
        // Deterministic recency: the non-rules crowder is the NEWEST row,
        // the open rule the OLDEST — a created_at-capped query that filters
        // the page only afterwards would drop the rule at limit 1.
        for (id, created) in [
            (SAMPLE_UUID_V7_1, "2026-06-01T00:00:00Z"),
            (SAMPLE_UUID_V7_2, "2026-06-02T00:00:00Z"),
            (SAMPLE_UUID_V7_3, "2026-06-03T00:00:00Z"),
            (SAMPLE_UUID_V7_4, "2026-06-04T00:00:00Z"),
        ] {
            sqlx::query("UPDATE fact_index SET created_at = ? WHERE fact_id = ?")
                .bind(created)
                .bind(id)
                .execute(&pool)
                .await
                .expect("backdate");
        }

        let bob = "user:bob".parse::<Principal>().unwrap();
        let rules = find_behaviour_rules(&pool, "agent", &bob, at, 1)
            .await
            .expect("query");
        assert_eq!(
            rules.iter().map(|r| r.fact_id.as_str()).collect::<Vec<_>>(),
            vec![SAMPLE_UUID_V7_1],
            "limit 1 must still serve the old open rule: the newer non-rules \
             fact, the closed rule, and the other owner's rule never enter \
             the window"
        );

        // The closed rule is served again once the asked instant precedes
        // its `valid_to` (the window-contains-instant shape of `valid_at`).
        let before_close = find_behaviour_rules(&pool, "agent", &bob, "2026-06-30T00:00:00Z", 0)
            .await
            .expect("query");
        assert_eq!(
            before_close.len(),
            2,
            "both of bob's rules are in force before the closure instant"
        );
    }

    #[tokio::test]
    async fn move_to_wiki_repoints_wiki_and_path_preserving_identity() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "I love pasta");
        insert(&pool, &f).await.expect("insert");

        let touched = move_to_wiki(
            &pool,
            &f.fact_id,
            "bob",
            "wikis/bob/cooking.md",
            Some(10),
            Some(74),
        )
        .await
        .expect("move");
        assert_eq!(touched, 1);

        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        // wiki_id + source_path + offsets moved.
        assert_eq!(back.wiki_id, "bob");
        assert_eq!(back.source_path, "wikis/bob/cooking.md");
        assert_eq!(back.region_start, Some(10));
        assert_eq!(back.region_end, Some(74));
        // Identity, body, ACL and attribution preserved.
        assert_eq!(back.fact_id, f.fact_id);
        assert_eq!(back.text, "I love pasta");
        assert_eq!(back.owner_id, "user:alice".parse().unwrap());
        assert_eq!(back.allow_ids, vec!["group:family".parse().unwrap()]);
        assert_eq!(back.sender_id, Some("user:bob".parse().unwrap()));
    }

    #[tokio::test]
    async fn move_to_wiki_no_ops_on_tombstoned_row() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "gone");
        insert(&pool, &f).await.expect("insert");
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("tombstone");

        let touched = move_to_wiki(&pool, &f.fact_id, "bob", "wikis/bob/x.md", None, None)
            .await
            .expect("move");
        assert_eq!(touched, 0, "tombstoned rows must not move");
    }

    #[tokio::test]
    async fn latest_page_activity_reads_active_facts_only() {
        let pool = make_pool().await;
        let a = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "fact a");
        let b = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "fact b");
        insert(&pool, &a).await.expect("insert a");
        insert(&pool, &b).await.expect("insert b");

        // A page with no facts yields no freshness signal.
        assert_eq!(
            latest_page_activity(&pool, "alice", "wikis/alice/other.md")
                .await
                .expect("query"),
            None
        );
        assert!(
            latest_page_activity(&pool, "alice", "wikis/alice/intro.md")
                .await
                .expect("query")
                .is_some()
        );

        // Superseding `b` bumps its own updated_at — exactly the value
        // that must NOT leak: the freshness pool is active facts only,
        // so the signal collapses to `a`'s timestamp.
        mark_superseded(&pool, &b.fact_id, &a.fact_id)
            .await
            .expect("supersede");
        let after = latest_page_activity(&pool, "alice", "wikis/alice/intro.md")
            .await
            .expect("query")
            .expect("a is still active");
        let row_a = find_by_id(&pool, &a.fact_id)
            .await
            .expect("find")
            .expect("row a");
        assert_eq!(after, row_a.updated_at);
    }

    #[tokio::test]
    async fn page_acl_map_keeps_superseded_rows_and_decodes_principals() {
        let pool = make_pool().await;
        let a = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "fact a");
        let b = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "fact b");
        // Different file (its own wiki's page) → excluded by source_path.
        let other = sample_new_fact(SAMPLE_UUID_V7_3, "bob", "user:bob", "other wiki");
        // A foreign-home fact woven into alice's page by the narrative
        // compiler: home `wiki_id = famiglia`, but its bytes live in
        // `wikis/alice/intro.md`. The map keys on `source_path` (the file),
        // so this MUST be loaded — keying on `wiki_id` too would drop it and
        // redact it for everyone, including its owner.
        let mut embedded = sample_new_fact(SAMPLE_UUID_V7_4, "famiglia", "user:alice", "embedded");
        embedded.source_path = "wikis/alice/intro.md".to_owned();
        insert(&pool, &a).await.expect("insert a");
        insert(&pool, &b).await.expect("insert b");
        insert(&pool, &other).await.expect("insert other");
        insert(&pool, &embedded).await.expect("insert embedded");

        // Superseded rows stay in the map: whatever region text is still
        // on disk keeps its last-known gate instead of falling back to
        // the page default.
        mark_superseded(&pool, &b.fact_id, &a.fact_id)
            .await
            .expect("supersede");

        let map = page_acl_map(&pool, "wikis/alice/intro.md")
            .await
            .expect("load");
        assert_eq!(
            map.len(),
            3,
            "both alice rows + the embedded famiglia row; the bob-file row excluded"
        );
        let rec = map.get(&a.fact_id).expect("a present");
        assert_eq!(rec.owner, "user:alice".parse().unwrap());
        assert_eq!(rec.allow, vec!["group:family".parse().unwrap()]);
        assert_eq!(rec.sender, Some("user:bob".parse().unwrap()));
        assert!(map.contains_key(&b.fact_id), "superseded row kept");
        assert!(
            map.contains_key(&embedded.fact_id),
            "foreign-wiki region in this file is loaded by source_path"
        );

        let empty = page_acl_map(&pool, "wikis/alice/other.md")
            .await
            .expect("load");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn page_acl_map_active_drops_superseded_and_deleted() {
        // The reader/redaction variant excludes retired rows so a marker
        // left on disk after its fact was superseded/deleted redacts
        // fail-closed (bare marker → no ACL in the map → unreadable).
        let pool = make_pool().await;
        let a = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "fact a");
        let b = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "fact b");
        let c = sample_new_fact(SAMPLE_UUID_V7_3, "alice", "user:alice", "fact c");
        insert(&pool, &a).await.expect("insert a");
        insert(&pool, &b).await.expect("insert b");
        insert(&pool, &c).await.expect("insert c");
        mark_superseded(&pool, &b.fact_id, &a.fact_id)
            .await
            .expect("supersede b");
        mark_forgotten(&pool, &c.fact_id, "user_request")
            .await
            .expect("forget c");

        let active = page_acl_map_active(&pool, "wikis/alice/intro.md")
            .await
            .expect("load active");
        assert_eq!(active.len(), 1, "only the live fact survives");
        assert!(active.contains_key(&a.fact_id));
        assert!(!active.contains_key(&b.fact_id), "superseded dropped");
        assert!(!active.contains_key(&c.fact_id), "deleted dropped");

        // The full map (interchange/export) still carries all three.
        let full = page_acl_map(&pool, "wikis/alice/intro.md")
            .await
            .expect("load full");
        assert_eq!(full.len(), 3, "full map keeps retired rows for export");
    }

    #[tokio::test]
    async fn insert_defaults_validity_to_open() {
        // The common case: no horizon known at capture → both ends NULL
        // ("true now"), which is what the old "knowledge" regime meant.
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "likes pasta");
        insert(&pool, &f).await.expect("insert");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.valid_from.is_none());
        assert!(back.valid_to.is_none());
        assert!(back.decay_reason.is_none());
    }

    #[tokio::test]
    async fn insert_preserves_validity_interval_and_leaves_decay_reason_null() {
        // A fact with a known horizon (e.g. an appointment) round-trips its
        // interval. `decay_reason` stays NULL: it is only stamped when a decay
        // trigger *closes* valid_to, never at insert — even when valid_to is a
        // known future date.
        let pool = make_pool().await;
        let mut f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "meeting thu 17:00");
        f.valid_from = Some("2026-06-06T00:00:00Z".to_owned());
        f.valid_to = Some("2026-06-11T17:00:00Z".to_owned());
        insert(&pool, &f).await.expect("insert");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(back.valid_from.as_deref(), Some("2026-06-06T00:00:00Z"));
        assert_eq!(back.valid_to.as_deref(), Some("2026-06-11T17:00:00Z"));
        assert!(back.decay_reason.is_none());
    }

    #[tokio::test]
    async fn close_validity_stamps_and_restore_round_trips() {
        // The closure verb: stamp valid_to + decay_reason (+ the successor
        // pointer when the closer knows it) on an open fact, get the
        // previous values back for the receipt, and a restore from that
        // snapshot reopens the window exactly as it was.
        let pool = make_pool().await;
        let f = sample_new_fact(
            SAMPLE_UUID_V7_1,
            "alice",
            "user:alice",
            "wants to watch Jumanji",
        );
        insert(&pool, &f).await.expect("insert");
        let successor = FactId::parse(SAMPLE_UUID_V7_2).unwrap();

        let prev = close_validity(
            &pool,
            &f.fact_id,
            "2026-06-11T20:00:00Z",
            decay::COMPLETED,
            Some(&successor),
        )
        .await
        .expect("close")
        .expect("active row");
        assert_eq!(
            prev,
            ClosedValidity {
                prev_valid_to: None,
                prev_decay_reason: None,
                prev_successor_fact_id: None
            }
        );
        let closed = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(closed.valid_to.as_deref(), Some("2026-06-11T20:00:00Z"));
        assert_eq!(closed.decay_reason.as_deref(), Some(decay::COMPLETED));
        assert_eq!(closed.successor_fact_id.as_ref(), Some(&successor));

        let touched = restore_validity(
            &pool,
            &f.fact_id,
            prev.prev_valid_to.as_deref(),
            prev.prev_decay_reason.as_deref(),
            prev.prev_successor_fact_id.as_ref().map(FactId::as_str),
        )
        .await
        .expect("restore");
        assert_eq!(touched, 1);
        let reopened = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert!(reopened.valid_to.is_none());
        assert!(reopened.decay_reason.is_none());
        assert!(
            reopened.successor_fact_id.is_none(),
            "the revert clears the pointer the closure stamped"
        );
    }

    #[tokio::test]
    async fn close_validity_without_successor_keeps_an_earlier_pointer() {
        // A None successor never wipes a pointer an earlier closure stamped
        // (the COALESCE half of the contract).
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "meal plan v1");
        insert(&pool, &f).await.expect("insert");
        let successor = FactId::parse(SAMPLE_UUID_V7_2).unwrap();
        close_validity(
            &pool,
            &f.fact_id,
            "2026-06-11T20:00:00Z",
            decay::CONTRADICTED,
            Some(&successor),
        )
        .await
        .expect("close")
        .expect("active row");

        let prev = close_validity(
            &pool,
            &f.fact_id,
            "2026-06-12T08:00:00Z",
            decay::RETRACTED,
            None,
        )
        .await
        .expect("re-close")
        .expect("active row");
        assert_eq!(
            prev.prev_successor_fact_id.as_ref(),
            Some(&successor),
            "the snapshot captures the earlier pointer"
        );
        let row = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(
            row.successor_fact_id.as_ref(),
            Some(&successor),
            "a successor-less re-closure leaves the pointer standing"
        );
    }

    #[tokio::test]
    async fn close_validity_skips_unknown_and_tombstoned_rows() {
        // The closure verb returns None (caller skips) instead of failing the
        // turn: an unknown id, or a row a forget already tombstoned.
        let pool = make_pool().await;
        let phantom = FactId::parse(SAMPLE_UUID_V7_2).unwrap();
        assert!(
            close_validity(
                &pool,
                &phantom,
                "2026-06-11T00:00:00Z",
                decay::RETRACTED,
                None
            )
            .await
            .expect("query")
            .is_none()
        );
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "serra project");
        insert(&pool, &f).await.expect("insert");
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("forget");
        assert!(
            close_validity(
                &pool,
                &f.fact_id,
                "2026-06-11T00:00:00Z",
                decay::RETRACTED,
                None
            )
            .await
            .expect("query")
            .is_none()
        );
    }

    #[tokio::test]
    async fn set_validity_corrects_one_bound_and_leaves_the_other_and_decay() {
        // The validity-edit verb: setting only valid_to leaves valid_from
        // untouched and NEVER stamps decay_reason (a date correction is not
        // a closure). A restore from the snapshot puts both bounds back.
        let pool = make_pool().await;
        let mut f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "milk expires");
        f.valid_from = Some("2026-06-10T00:00:00Z".to_owned());
        f.valid_to = Some("2026-06-25T00:00:00Z".to_owned());
        insert(&pool, &f).await.expect("insert");

        // Correct only valid_to (the 25th was wrong; it's the 20th).
        let prev = set_validity(&pool, &f.fact_id, None, Some("2026-06-20T00:00:00Z"))
            .await
            .expect("edit")
            .expect("active row");
        assert_eq!(
            prev,
            PrevValidity {
                prev_valid_from: Some("2026-06-10T00:00:00Z".to_owned()),
                prev_valid_to: Some("2026-06-25T00:00:00Z".to_owned()),
            }
        );
        let edited = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(
            edited.valid_from.as_deref(),
            Some("2026-06-10T00:00:00Z"),
            "the omitted bound is left unchanged"
        );
        assert_eq!(edited.valid_to.as_deref(), Some("2026-06-20T00:00:00Z"));
        assert!(
            edited.decay_reason.is_none(),
            "a date correction never stamps decay_reason"
        );

        // Restore both bounds from the snapshot.
        let touched = restore_validity_interval(
            &pool,
            &f.fact_id,
            prev.prev_valid_from.as_deref(),
            prev.prev_valid_to.as_deref(),
        )
        .await
        .expect("restore");
        assert_eq!(touched, 1);
        let back = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(back.valid_from.as_deref(), Some("2026-06-10T00:00:00Z"));
        assert_eq!(back.valid_to.as_deref(), Some("2026-06-25T00:00:00Z"));
    }

    #[tokio::test]
    async fn set_validity_skips_unknown_and_tombstoned_rows() {
        let pool = make_pool().await;
        let phantom = FactId::parse(SAMPLE_UUID_V7_2).unwrap();
        assert!(
            set_validity(&pool, &phantom, None, Some("2026-06-20T00:00:00Z"))
                .await
                .expect("query")
                .is_none()
        );
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "serra");
        insert(&pool, &f).await.expect("insert");
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("forget");
        assert!(
            set_validity(&pool, &f.fact_id, None, Some("2026-06-20T00:00:00Z"))
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn set_acl_replaces_and_restore_round_trips() {
        // The acl-change verb: replace owner/allow/sender, get the previous
        // snapshot back, and a restore from it reinstates the prior ACL.
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "private note");
        // sample_new_fact seeds allow=[group:family], sender=user:bob.
        insert(&pool, &f).await.expect("insert");

        let new_owner: Principal = "user:alice".parse().unwrap();
        let new_allow = vec!["global".parse::<Principal>().unwrap()];
        let prev = set_acl(&pool, &f.fact_id, &new_owner, &new_allow, None)
            .await
            .expect("set")
            .expect("active row");
        assert_eq!(prev.prev_owner_id, "user:alice".parse().unwrap());
        assert_eq!(prev.prev_allow_ids, vec!["group:family".parse().unwrap()]);
        assert_eq!(prev.prev_sender_id, Some("user:bob".parse().unwrap()));

        let changed = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(changed.allow_ids, vec!["global".parse().unwrap()]);
        assert_eq!(changed.sender_id, None, "sender cleared when None passed");

        let touched = restore_acl(
            &pool,
            &f.fact_id,
            &prev.prev_owner_id,
            &prev.prev_allow_ids,
            prev.prev_sender_id.as_ref(),
        )
        .await
        .expect("restore");
        assert_eq!(touched, 1);
        let back = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(back.allow_ids, vec!["group:family".parse().unwrap()]);
        assert_eq!(back.sender_id, Some("user:bob".parse().unwrap()));
    }

    #[tokio::test]
    async fn mark_superseded_closes_the_predecessors_window_as_contradicted() {
        // The supersede chokepoint is also the contradiction closure: the
        // predecessor's open window closes at supersede time with
        // decay_reason = contradicted.
        let pool = make_pool().await;
        let old = sample_new_fact(
            SAMPLE_UUID_V7_1,
            "alice",
            "user:alice",
            "drives an old Panda",
        );
        let new = sample_new_fact(
            SAMPLE_UUID_V7_2,
            "alice",
            "user:alice",
            "drives a white Tesla",
        );
        insert(&pool, &old).await.expect("insert old");
        insert(&pool, &new).await.expect("insert new");
        mark_superseded(&pool, &old.fact_id, &new.fact_id)
            .await
            .expect("supersede");
        let back = find_by_id(&pool, &old.fact_id).await.unwrap().unwrap();
        assert!(back.valid_to.is_some(), "open window closed at supersede");
        assert_eq!(back.decay_reason.as_deref(), Some(decay::CONTRADICTED));
    }

    #[tokio::test]
    async fn mark_superseded_never_extends_an_earlier_concrete_end() {
        // A dated commitment superseded AFTER its own end keeps the earlier
        // valid_to (COALESCE: the closure is monotone, never an extension);
        // an already-stamped reason is preserved too.
        let pool = make_pool().await;
        let mut old = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "dentist thu 17:00");
        old.valid_to = Some("2026-06-04T17:00:00Z".to_owned());
        let new = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "dentist fri 9:00");
        insert(&pool, &old).await.expect("insert old");
        insert(&pool, &new).await.expect("insert new");
        stamp_decay_reason(&pool, &old.fact_id, decay::COMPLETED)
            .await
            .expect("stamp");
        mark_superseded(&pool, &old.fact_id, &new.fact_id)
            .await
            .expect("supersede");
        let back = find_by_id(&pool, &old.fact_id).await.unwrap().unwrap();
        assert_eq!(back.valid_to.as_deref(), Some("2026-06-04T17:00:00Z"));
        assert_eq!(back.decay_reason.as_deref(), Some(decay::COMPLETED));
    }

    #[tokio::test]
    async fn insert_defaults_salience_to_unspecified() {
        // Most facts carry no salience → NULL (treated as `normal` downstream).
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "likes pasta");
        insert(&pool, &f).await.expect("insert");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.salience.is_none());
    }

    #[tokio::test]
    async fn insert_preserves_salience() {
        // An always-on fact (identity / health) carries `high`, and the
        // insert round-trips it so it can be routed to the actor-wiki
        // index.md base context.
        let pool = make_pool().await;
        let mut f = sample_new_fact(SAMPLE_UUID_V7_1, "galadriel", "user:galadriel", "coeliac");
        f.salience = Some("high".to_owned());
        insert(&pool, &f).await.expect("insert");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(back.salience.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn insert_duplicate_fact_id_errors() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.expect("first insert");
        let err = insert(&pool, &f).await.expect_err("must fail");
        assert!(matches!(err, FactIndexError::Db(_)));
    }

    #[tokio::test]
    async fn insert_if_absent_skips_duplicate_without_error() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "first body");
        let n = insert_if_absent(&pool, &f).await.expect("first insert");
        assert_eq!(n, 1, "first insert must materialise one row");

        // Second insert with the same `fact_id` but a different body must
        // silently no-op and preserve the original row verbatim.
        let mut duplicate = f.clone();
        duplicate.text = "second body".to_owned();
        let n = insert_if_absent(&pool, &duplicate)
            .await
            .expect("second insert");
        assert_eq!(n, 0, "duplicate insert must report zero rows touched");

        let row = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("row exists");
        assert_eq!(
            row.text, "first body",
            "the original row must remain unchanged on conflict"
        );
    }

    #[tokio::test]
    async fn insert_if_absent_treats_tombstoned_row_as_existing() {
        // The reindex pipeline relies on `insert_if_absent` to absorb
        // the case where the marker on disk references a fact that has
        // been tombstoned in the DB (deleted_at IS NOT NULL). The PRIMARY
        // KEY conflict still applies — the fact_id is still occupied.
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .unwrap();
        let n = insert_if_absent(&pool, &f).await.expect("insert");
        assert_eq!(n, 0);
        // Tombstone preserved — `insert_if_absent` must never resurrect.
        let row = find_by_id(&pool, &f.fact_id).await.unwrap().expect("row");
        assert!(row.deleted_at.is_some());
    }

    #[tokio::test]
    async fn find_by_id_unknown_returns_none() {
        let pool = make_pool().await;
        let id = FactId::parse(SAMPLE_UUID_V7_1).unwrap();
        let res = find_by_id(&pool, &id).await.expect("find");
        assert!(res.is_none());
    }

    // ---------- supersede / forget ----------

    #[tokio::test]
    async fn mark_superseded_sets_columns_and_excludes_from_active() {
        let pool = make_pool().await;
        let old = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old text");
        let new = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new text");
        insert(&pool, &old).await.unwrap();
        insert(&pool, &new).await.unwrap();
        let touched = mark_superseded(&pool, &old.fact_id, &new.fact_id)
            .await
            .expect("supersede");
        assert_eq!(touched, 1);
        let active = find_active_in_wiki(&pool, "alice").await.expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].fact_id, new.fact_id);
        let back = find_by_id(&pool, &old.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.superseded_at.is_some());
        assert_eq!(back.superseded_by, Some(new.fact_id));
    }

    #[tokio::test]
    async fn mark_superseded_no_op_when_already_superseded() {
        let pool = make_pool().await;
        let old = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old");
        let new = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new");
        insert(&pool, &old).await.unwrap();
        insert(&pool, &new).await.unwrap();
        mark_superseded(&pool, &old.fact_id, &new.fact_id)
            .await
            .unwrap();
        let again = mark_superseded(&pool, &old.fact_id, &new.fact_id)
            .await
            .expect("supersede twice");
        assert_eq!(again, 0, "second call must be a no-op");
    }

    #[tokio::test]
    async fn mark_forgotten_sets_tombstone_and_excludes_from_active() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        let touched = mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("forget");
        assert_eq!(touched, 1);
        let active = find_active_in_wiki(&pool, "alice").await.expect("active");
        assert!(active.is_empty(), "forgotten row must drop out of active");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.deleted_at.is_some());
        assert_eq!(back.deleted_reason, Some("user_request".to_owned()));
    }

    #[tokio::test]
    async fn restore_forgotten_brings_a_tombstoned_row_back_to_active() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("forget");

        let touched = restore_forgotten(&pool, &f.fact_id).await.expect("restore");
        assert_eq!(touched, 1, "the tombstoned row is restored");
        let active = find_active_in_wiki(&pool, "alice").await.expect("active");
        assert_eq!(active.len(), 1, "the row rejoins the active set");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.deleted_at.is_none(), "deleted_at cleared");
        assert!(back.deleted_reason.is_none(), "deleted_reason cleared");

        // Restoring an already-live row is a no-op.
        let again = restore_forgotten(&pool, &f.fact_id)
            .await
            .expect("restore twice");
        assert_eq!(again, 0, "restoring a live row touches nothing");
    }

    #[tokio::test]
    async fn mark_forgotten_is_idempotent() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .unwrap();
        let again = mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .expect("forget twice");
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn mark_forgotten_at_tombstones_when_row_still_lives_there() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        let touched = mark_forgotten_at(&pool, &f.fact_id, "wikis/alice/intro.md", "test_reason")
            .await
            .expect("forget at");
        assert_eq!(touched, 1);
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(back.deleted_at.is_some());
    }

    #[tokio::test]
    async fn mark_forgotten_at_spares_a_row_that_moved_away() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        // A concurrent promote repointed the row to another page after
        // the sweep took its snapshot.
        move_region(&pool, &f.fact_id, "wikis/alice/other.md", Some(0), Some(64))
            .await
            .unwrap();
        let touched = mark_forgotten_at(&pool, &f.fact_id, "wikis/alice/intro.md", "test_reason")
            .await
            .expect("forget at");
        assert_eq!(touched, 0, "a moved row is not an orphan of the old page");
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert!(
            back.deleted_at.is_none(),
            "row must survive the stale sweep"
        );
    }

    // ---------- recall hits ----------

    #[tokio::test]
    async fn bump_recall_hits_increments_counter_and_stamps_time() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        let updated = bump_recall_hits(&pool, std::slice::from_ref(&f.fact_id))
            .await
            .expect("bump");
        assert_eq!(updated, 1);
        let back = find_by_id(&pool, &f.fact_id)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(back.recall_count_30d, 1);
        assert!(back.last_recall_at.is_some());
    }

    #[tokio::test]
    async fn bump_recall_hits_empty_input_is_noop() {
        let pool = make_pool().await;
        let updated = bump_recall_hits(&pool, &[]).await.expect("bump");
        assert_eq!(updated, 0);
    }

    // ---------- source-path bulk ops ----------

    #[tokio::test]
    async fn drop_by_source_path_removes_only_matching_rows() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "a");
        let mut f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "b");
        f2.source_path = "wikis/alice/recipes.md".into();
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        let removed = drop_by_source_path(&pool, "wikis/alice/intro.md")
            .await
            .expect("drop");
        assert_eq!(removed, 1);
        assert!(find_by_id(&pool, &f1.fact_id).await.unwrap().is_none());
        assert!(find_by_id(&pool, &f2.fact_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn distinct_source_paths_returns_set() {
        let pool = make_pool().await;
        let mut f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "a");
        let mut f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "b");
        f1.source_path = "wikis/alice/intro.md".into();
        f2.source_path = "wikis/alice/recipes.md".into();
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        let paths = distinct_source_paths(&pool, "alice").await.expect("paths");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("wikis/alice/intro.md"));
        assert!(paths.contains("wikis/alice/recipes.md"));
    }

    #[tokio::test]
    async fn count_active_in_wiki_ignores_tombstones() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "a");
        let f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "b");
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        assert_eq!(count_active_in_wiki(&pool, "alice").await.unwrap(), 2);
        mark_forgotten(&pool, &f1.fact_id, "user_request")
            .await
            .unwrap();
        assert_eq!(count_active_in_wiki(&pool, "alice").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn wiki_visible_to_gates_on_derived_visibility() {
        let pool = make_pool().await;
        // Wiki "alice" holds one fact: owner alice, allow-listed to group:team.
        let mut f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "private");
        f.allow_ids = vec!["group:team".parse().unwrap()];
        f.sender_id = Some("user:alice".parse().unwrap());
        insert(&pool, &f).await.unwrap();

        // The owner reads it → the wiki surfaces.
        assert!(wiki_visible_to(&pool, "alice", "alice", &[]).await.unwrap());
        // A team member reads it via the per-fragment `allow=` grant — the gate
        // uses `can_read`, so an allow-listed reader is NOT excluded (the whole
        // reason it is not gated on the wiki-level `shared_with`).
        assert!(
            wiki_visible_to(&pool, "alice", "bob", &["team".to_owned()])
                .await
                .unwrap()
        );
        // A NON-empty wiki whose only fact the reader cannot read does NOT
        // surface (404 upstream) — this is the leak the gate closes.
        assert!(!wiki_visible_to(&pool, "alice", "carol", &[]).await.unwrap());
        // A wiki with NO active facts hides nothing → it surfaces to anyone (a
        // fresh / not-yet-promoted wiki must not 404 for its owner).
        assert!(wiki_visible_to(&pool, "empty", "alice", &[]).await.unwrap());
    }

    #[tokio::test]
    async fn move_region_bumps_source_path_and_offsets() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "fact body");
        insert(&pool, &f).await.unwrap();
        let touched = move_region(
            &pool,
            &f.fact_id,
            "wikis/alice/giardinaggio.md",
            Some(0),
            Some(64),
        )
        .await
        .unwrap();
        assert_eq!(touched, 1);
        let row = find_by_id(&pool, &f.fact_id).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/giardinaggio.md");
        assert_eq!(row.region_start, Some(0));
        assert_eq!(row.region_end, Some(64));
        // Body, embedding, attribution preserved.
        assert_eq!(row.text, "fact body");
        assert_eq!(row.embedding, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(row.owner_id.to_string(), "user:alice");
    }

    #[tokio::test]
    async fn move_region_is_no_op_on_tombstoned_row() {
        let pool = make_pool().await;
        let f = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "x");
        insert(&pool, &f).await.unwrap();
        mark_forgotten(&pool, &f.fact_id, "user_request")
            .await
            .unwrap();
        let touched = move_region(&pool, &f.fact_id, "wikis/alice/other.md", Some(0), Some(10))
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }

    #[tokio::test]
    async fn move_region_is_no_op_on_superseded_row() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old");
        let f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new");
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        mark_superseded(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        let touched = move_region(
            &pool,
            &f1.fact_id,
            "wikis/alice/other.md",
            Some(0),
            Some(10),
        )
        .await
        .unwrap();
        assert_eq!(touched, 0);
    }

    #[tokio::test]
    async fn clear_supersede_reactivates_when_chain_still_matches() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old");
        let f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new");
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        mark_superseded(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();

        let touched = clear_supersede(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        assert_eq!(touched, 1);

        let row = find_by_id(&pool, &f1.fact_id).await.unwrap().unwrap();
        assert!(row.superseded_at.is_none());
        assert!(row.superseded_by.is_none());
    }

    #[tokio::test]
    async fn clear_supersede_is_no_op_when_chain_has_moved_on() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old");
        let f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new");
        let f3_id = "018f1234-5678-7abc-9def-0123456789ad";
        let f3 = sample_new_fact(f3_id, "alice", "user:alice", "newer");
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        insert(&pool, &f3).await.unwrap();
        // f1 → f2 first; then we manually re-activate f1 and re-supersede
        // it by f3 to simulate "chain has moved on past our pair".
        mark_superseded(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE fact_index SET superseded_at = NULL, superseded_by = NULL WHERE fact_id = ?",
        )
        .bind(f1.fact_id.as_str())
        .execute(&pool)
        .await
        .unwrap();
        mark_superseded(&pool, &f1.fact_id, &f3.fact_id)
            .await
            .unwrap();

        let touched = clear_supersede(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        assert_eq!(touched, 0);

        let row = find_by_id(&pool, &f1.fact_id).await.unwrap().unwrap();
        assert!(row.superseded_at.is_some());
        assert_eq!(row.superseded_by.as_ref().map(FactId::as_str), Some(f3_id));
    }

    #[tokio::test]
    async fn rebase_source_path_prefix_rewrites_matching_rows() {
        let pool = make_pool().await;
        let mut f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "a");
        f1.source_path = "wikis/alice/intro.md".into();
        let mut f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "b");
        f2.source_path = "wikis/alice/notes.md".into();
        // A row that must NOT match — sibling with shared prefix bytes.
        let mut f3 = sample_new_fact(SAMPLE_UUID_V7_3, "alice-bis", "user:alice", "c");
        f3.source_path = "wikis/alice-bis/intro.md".into();
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        insert(&pool, &f3).await.unwrap();

        let touched = rebase_source_path_prefix(&pool, "wikis/alice/", "wikis/bob/family/alice/")
            .await
            .unwrap();
        assert_eq!(touched, 2, "only the two alice rows should rebase");

        let row1 = find_by_id(&pool, &f1.fact_id).await.unwrap().unwrap();
        assert_eq!(row1.source_path, "wikis/bob/family/alice/intro.md");
        let row2 = find_by_id(&pool, &f2.fact_id).await.unwrap().unwrap();
        assert_eq!(row2.source_path, "wikis/bob/family/alice/notes.md");
        // The sibling stays untouched — its prefix differs after the trailing slash.
        let row3 = find_by_id(&pool, &f3.fact_id).await.unwrap().unwrap();
        assert_eq!(row3.source_path, "wikis/alice-bis/intro.md");
    }

    #[tokio::test]
    async fn rebase_source_path_prefix_is_no_op_when_prefixes_match() {
        let pool = make_pool().await;
        let mut f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "a");
        f1.source_path = "wikis/alice/intro.md".into();
        insert(&pool, &f1).await.unwrap();
        let touched = rebase_source_path_prefix(&pool, "wikis/alice/", "wikis/alice/")
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }

    #[tokio::test]
    async fn clear_supersede_is_no_op_on_tombstoned_row() {
        let pool = make_pool().await;
        let f1 = sample_new_fact(SAMPLE_UUID_V7_1, "alice", "user:alice", "old");
        let f2 = sample_new_fact(SAMPLE_UUID_V7_2, "alice", "user:alice", "new");
        insert(&pool, &f1).await.unwrap();
        insert(&pool, &f2).await.unwrap();
        mark_superseded(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        mark_forgotten(&pool, &f1.fact_id, "user_request")
            .await
            .unwrap();

        let touched = clear_supersede(&pool, &f1.fact_id, &f2.fact_id)
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }
}
