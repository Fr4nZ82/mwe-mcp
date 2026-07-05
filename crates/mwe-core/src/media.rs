// SPDX-License-Identifier: AGPL-3.0-or-later
//! Media catalog + content-addressed blob store — the bytes behind
//! `{{embed=<catalog_id>}}` markers.
//!
//! The marker in a memory-wiki page is a **bare key**; everything behind
//! it — kind, MIME, size, caption and the per-media ACL — is authoritative
//! in the `media_catalog` table (the twin of `fact_index`), and the bytes
//! live in a global content-addressed store at
//! `<workdir>/media/<aa>/<sha256>` (sharded by the first two hex chars).
//! Identical bytes are stored once regardless of how many catalog rows or
//! wikis reference them. Design SSOT:
//! [media pipeline](../../../wiki/design-notes/media-pipeline.md); table
//! DDL in [`migrations/0039_media_catalog.sql`](../../../migrations/0039_media_catalog.sql).
//!
//! Write ordering is load-bearing for the workdir snapshot
//! ([backup](../../../wiki/design-notes/backup-and-dr.md)): the blob is
//! written **before** the catalog row, so a row present in a snapshot's DB
//! image always finds its blob in the later file copy. An orphan blob
//! without a row is harmless garbage; a row without a blob would be a
//! broken `GET` — the order rules it out.
//!
//! ACL semantics: the row's `owner` is stamped from the authenticated
//! (act-as-resolved) principal at upload and never changes; when ingest
//! files a fact embedding the media, the fact's read set is **unioned**
//! into `allow_ids` ([`widen_acl`]) — monotone widening only, mirroring
//! the `allow`-monotonicity invariant of `acl::can_read`.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use thiserror::Error;

use crate::acl::can_read;
use crate::fact_index::{principals_from_json, principals_to_json};
use crate::types::{Acl, CatalogId, Principal};

/// Errors raised by the media layer.
#[derive(Debug, Error)]
pub enum MediaError {
    /// Underlying `SQLite` error.
    #[error("media_catalog db: {0}")]
    Db(#[from] sqlx::Error),

    /// JSON serialization failure on `allow_ids`.
    #[error("media_catalog json: {0}")]
    Json(#[from] serde_json::Error),

    /// Blob store I/O failure.
    #[error("media store io: {0}")]
    Io(#[from] std::io::Error),

    /// The upload declared a kind outside the canonical vocabulary.
    #[error("invalid media kind {0:?}: expected photo | video | audio | doc")]
    InvalidKind(String),

    /// The upload carried zero bytes.
    #[error("empty media payload")]
    EmptyPayload,

    /// A stored row failed to decode (corrupt principal / catalog id).
    #[error("media_catalog decode: {0}")]
    Decode(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, MediaError>;

/// Canonical media kinds — the vocabulary fixed at minting time.
///
/// The `CatalogId` *parser* deliberately accepts any `[a-z]+` kind so
/// legacy ids and imported archives stay valid input forever; only this
/// producer-side vocabulary is closed (same convention as
/// `fact_index::decay`).
pub mod kind {
    /// Full pipeline: stored, vision-described, filed as a fact.
    pub const PHOTO: &str = "photo";
    /// Archived with its caption only (no video understanding in v1).
    pub const VIDEO: &str = "video";
    /// Transcript-is-the-fact; the file rides as optional attachment.
    pub const AUDIO: &str = "audio";
    /// Generic document attachment.
    pub const DOC: &str = "doc";

    /// All canonical kinds, for validation and schema enums.
    pub const ALL: [&str; 4] = [PHOTO, VIDEO, AUDIO, DOC];

    /// Whether `s` is one of the canonical kinds.
    #[must_use]
    pub fn is_canonical(s: &str) -> bool {
        ALL.contains(&s)
    }
}

/// One row of the `media_catalog` table, wire types decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRow {
    /// The minted `c-YYYY-MM-DD-<kind>-NNN.<ext>` key.
    pub catalog_id: CatalogId,
    /// Lowercase hex sha256 of the blob (the content address).
    pub sha256: String,
    /// Canonical kind (`photo` / `video` / `audio` / `doc`).
    pub kind: String,
    /// MIME type served as `Content-Type` by `GET /media`.
    pub mime: String,
    /// Blob size in bytes.
    pub size_bytes: i64,
    /// Owning principal — explicit, never inherited.
    pub owner_id: Principal,
    /// Additional principals granted read (grows by [`widen_acl`]).
    pub allow_ids: Vec<Principal>,
    /// Cross-user attribution. Materialized to `owner` at upload (media has
    /// no distinct capturer today); `None` survives only as the degenerate
    /// scrubbed state that falls back to owner at read time.
    pub sender_id: Option<Principal>,
    /// The consumer that performed the upload, for audit.
    pub uploaded_by_consumer: Option<String>,
    /// Caption supplied at upload or ingest.
    pub caption: Option<String>,
    /// Consumer-supplied description (smart consumer / host recognizer).
    pub description: Option<String>,
    /// Original client filename from the multipart part.
    pub original_filename: Option<String>,
    /// Wall-clock at insert (ISO 8601, UTC).
    pub created_at: String,
    /// Wall-clock at last update (ISO 8601, UTC).
    pub updated_at: String,
}

/// Payload for [`store_media`].
#[derive(Debug, Clone)]
pub struct NewMedia {
    /// Raw bytes of the media file.
    pub bytes: Vec<u8>,
    /// Canonical kind; anything else is [`MediaError::InvalidKind`].
    pub kind: String,
    /// Declared MIME type; empty falls back to `application/octet-stream`.
    pub mime: String,
    /// Owning principal (the act-as-resolved authenticated sender).
    pub owner: Principal,
    /// The uploading consumer's id (token claim), for audit.
    pub uploaded_by_consumer: Option<String>,
    /// Optional caption.
    pub caption: Option<String>,
    /// Optional consumer-supplied description.
    pub description: Option<String>,
    /// Original client filename, used to derive the catalog-id extension.
    pub original_filename: Option<String>,
}

/// Outcome of [`store_media`].
#[derive(Debug, Clone)]
pub struct StoreOutcome {
    /// The catalog row (fresh or pre-existing).
    pub row: MediaRow,
    /// `true` when the same bytes were already catalogued for the same
    /// owner and the existing row was returned instead of minting a new
    /// one (idempotent re-upload).
    pub deduplicated: bool,
}

/// Name of the blob-store directory under the workdir.
pub const MEDIA_DIR: &str = "media";

/// Whether `mime` is safe to serve inline on the server's origin.
///
/// Anything else must go out as `application/octet-stream` +
/// `Content-Disposition: attachment` so a stored HTML/SVG blob can
/// never become same-origin script. Shared by the bearer `GET /media`
/// and the dashboard's cookie-authenticated alias so the two mounts
/// cannot drift on the security call.
///
/// The decision runs on the **essence** (the type/subtype before any
/// `;` parameter, lowercased): the upload MIME is attacker-controlled
/// and `image/svg+xml; charset=utf-8` must fail exactly like the bare
/// form — a parameter must never widen the gate.
#[must_use]
pub fn inline_safe(mime: &str) -> bool {
    let essence = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    (essence.starts_with("image/") && essence != "image/svg+xml")
        || essence.starts_with("video/")
        || essence.starts_with("audio/")
        || essence == "application/pdf"
}

/// Absolute path of the blob for `sha256` under `workdir`.
///
/// Layout: `<workdir>/media/<aa>/<sha256>` where `aa` is the first two
/// hex chars — keeps any single directory small on large catalogs.
#[must_use]
pub fn blob_path(workdir: &Path, sha256: &str) -> PathBuf {
    let shard = sha256.get(..2).unwrap_or("00");
    workdir.join(MEDIA_DIR).join(shard).join(sha256)
}

/// Store one media item: content-address the bytes, write the blob (once
/// per hash), mint the catalog id and insert the row.
///
/// Idempotency: a second upload of the same bytes by the same owner
/// returns the existing row with `deduplicated = true` (absorbing bridge
/// retries and re-sent photos); the same bytes from a *different* owner
/// mint a fresh row sharing the blob.
///
/// # Errors
///
/// [`MediaError::InvalidKind`] / [`MediaError::EmptyPayload`] on bad
/// input; `Db` / `Io` / `Json` as labelled.
#[allow(clippy::too_many_lines)] // one sequential store: dedup probe → blob → mint+insert retry loop
pub async fn store_media(
    pool: &SqlitePool,
    workdir: &Path,
    media: NewMedia,
) -> Result<StoreOutcome> {
    if !kind::is_canonical(&media.kind) {
        return Err(MediaError::InvalidKind(media.kind));
    }
    if media.bytes.is_empty() {
        return Err(MediaError::EmptyPayload);
    }

    let sha256 = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&media.bytes))
    };

    // Idempotent re-upload: same bytes + same owner = the existing row.
    let owner_wire = media.owner.to_string();
    if let Some(row) = find_by_sha_and_owner(pool, &sha256, &owner_wire).await? {
        backfill_annotations(
            pool,
            &row,
            media.caption.as_deref(),
            media.description.as_deref(),
        )
        .await?;
        let row = find_by_id(pool, &row.catalog_id)
            .await?
            .ok_or_else(|| MediaError::Decode("dedup row vanished".into()))?;
        return Ok(StoreOutcome {
            row,
            deduplicated: true,
        });
    }

    // Blob first, row second — the snapshot ordering invariant (see the
    // module docs). Content-addressed blobs are immutable: if the path
    // already exists (same bytes from another owner) the write is skipped.
    let blob_abs = blob_path(workdir, &sha256);
    if !blob_abs.exists() {
        if let Some(parent) = blob_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = blob_abs.with_extension("mwe-write-in-progress");
        std::fs::write(&tmp, &media.bytes)?;
        std::fs::rename(&tmp, &blob_abs)?;
    }

    let mime = if media.mime.trim().is_empty() {
        "application/octet-stream".to_owned()
    } else {
        media.mime.trim().to_owned()
    };
    let ext = derive_ext(media.original_filename.as_deref(), &mime);
    let now = chrono::Utc::now();
    let allow_json = principals_to_json(&[])?;
    let now_str = now.to_rfc3339();
    let size = i64::try_from(media.bytes.len()).unwrap_or(i64::MAX);

    // Mint + insert with a small retry loop: two concurrent uploads can
    // race on the per-day-per-kind NNN (PK collision) or — for identical
    // bytes from the same owner — on the (sha256, owner) unique index.
    // Both surface as a unique-constraint violation; re-probe/re-mint
    // and try again instead of bubbling a 500.
    let mut catalog_id = None;
    for attempt in 0..3 {
        let candidate =
            mint_catalog_id(pool, &now.format("%Y-%m-%d").to_string(), &media.kind, &ext).await?;
        let inserted = sqlx::query(
            "INSERT INTO media_catalog (
                catalog_id, sha256, kind, mime, size_bytes,
                owner_id, allow_ids, sender_id, uploaded_by_consumer,
                caption, description, original_filename, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(candidate.as_str())
        .bind(&sha256)
        .bind(&media.kind)
        .bind(&mime)
        .bind(size)
        .bind(&owner_wire)
        .bind(&allow_json)
        // sender_id = owner: media has no distinct capturer concept today,
        // so provenance is materialized to the owner (never NULL-at-birth),
        // keeping it frozen if ownership is ever transferred.
        .bind(&owner_wire)
        .bind(&media.uploaded_by_consumer)
        .bind(&media.caption)
        .bind(&media.description)
        .bind(&media.original_filename)
        .bind(&now_str)
        .bind(&now_str)
        .execute(pool)
        .await;
        match inserted {
            Ok(_) => {
                catalog_id = Some(candidate);
                break;
            },
            Err(e) if is_unique_violation(&e) => {
                // A concurrent identical upload may have won the
                // (sha256, owner) race — return its row as a dedup.
                if let Some(row) = find_by_sha_and_owner(pool, &sha256, &owner_wire).await? {
                    return Ok(StoreOutcome {
                        row,
                        deduplicated: true,
                    });
                }
                tracing::debug!(attempt, "media: catalog-id mint collision — retrying");
            },
            Err(e) => return Err(e.into()),
        }
    }
    let Some(catalog_id) = catalog_id else {
        return Err(MediaError::Decode(
            "catalog-id minting kept colliding after 3 attempts".into(),
        ));
    };

    tracing::info!(
        catalog_id = %catalog_id,
        kind = %media.kind,
        size_bytes = size,
        owner = %owner_wire,
        "media stored"
    );

    let row = find_by_id(pool, &catalog_id)
        .await?
        .ok_or_else(|| MediaError::Decode("freshly inserted row missing".into()))?;
    Ok(StoreOutcome {
        row,
        deduplicated: false,
    })
}

/// Load one catalog row by key. `Ok(None)` for an unknown id.
///
/// # Errors
///
/// `Db` on query failure, `Decode` on a corrupt stored row.
pub async fn find_by_id(pool: &SqlitePool, catalog_id: &CatalogId) -> Result<Option<MediaRow>> {
    let raw: Option<RawMediaRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM media_catalog WHERE catalog_id = ?"
    ))
    .bind(catalog_id.as_str())
    .fetch_optional(pool)
    .await?;
    raw.map(decode_row).transpose()
}

/// The upload dedup probe: the row for this content hash owned by this
/// principal, when one exists.
async fn find_by_sha_and_owner(
    pool: &SqlitePool,
    sha256: &str,
    owner_wire: &str,
) -> Result<Option<MediaRow>> {
    let raw: Option<RawMediaRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM media_catalog WHERE sha256 = ? AND owner_id = ?"
    ))
    .bind(sha256)
    .bind(owner_wire)
    .fetch_optional(pool)
    .await?;
    raw.map(decode_row).transpose()
}

/// Load the catalog rows for `catalog_ids`, in the order given; unknown
/// ids are silently skipped (the caller counts them — export does).
///
/// # Errors
///
/// `Db` / `Decode` as in [`find_by_id`].
pub async fn find_by_ids(pool: &SqlitePool, catalog_ids: &[CatalogId]) -> Result<Vec<MediaRow>> {
    let mut out = Vec::with_capacity(catalog_ids.len());
    for id in catalog_ids {
        if let Some(row) = find_by_id(pool, id).await? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Whether `sender_id` (with `sender_groups`) may read this media item —
/// the same union rule as fact regions
/// ([redaction policy](../../../wiki/design-notes/redaction-policy.md)),
/// no admin bypass.
#[must_use]
pub fn row_visible_to(row: &MediaRow, sender_id: &str, sender_groups: &[String]) -> bool {
    let acl = Acl {
        owner: Some(row.owner_id.clone()),
        allow: row.allow_ids.clone(),
    };
    can_read(&acl, sender_id, sender_groups, row.sender_id.as_ref())
}

/// Widen the row's ACL with a linked fact's read set — **monotone union
/// only**, the media twin of the fact `allow`-monotonicity invariant.
///
/// The fact's owner, every `allow` entry and its sender are unioned into
/// the row's `allow_ids` (minus the row's own owner, which already
/// reads). The row's `owner`/`sender` slots never change. Widening is a
/// disclosure-relevant event, so it is traced; a dedicated audit verb is
/// the open ACL-change design (roadmap group 6) and deliberately not
/// built here.
///
/// Returns `true` when the row changed.
///
/// # Errors
///
/// `Db` / `Json` / `Decode` as labelled.
pub async fn widen_acl(
    pool: &SqlitePool,
    catalog_id: &CatalogId,
    fact_owner: &Principal,
    fact_allow: &[Principal],
    fact_sender: Option<&Principal>,
) -> Result<bool> {
    let Some(row) = find_by_id(pool, catalog_id).await? else {
        return Ok(false);
    };
    let mut allow = row.allow_ids.clone();
    let mut grew = false;
    let candidates = std::iter::once(fact_owner)
        .chain(fact_allow.iter())
        .chain(fact_sender);
    for p in candidates {
        if *p != row.owner_id && !allow.contains(p) {
            allow.push(p.clone());
            grew = true;
        }
    }
    if !grew {
        return Ok(false);
    }
    let allow_json = principals_to_json(&allow)?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE media_catalog SET allow_ids = ?, updated_at = ? WHERE catalog_id = ?")
        .bind(&allow_json)
        .bind(&now)
        .bind(catalog_id.as_str())
        .execute(pool)
        .await?;
    tracing::info!(
        catalog_id = %catalog_id,
        allow = %allow_json,
        "media ACL widened to a linked fact's read set"
    );
    Ok(true)
}

/// Stamp caption/description on the row when ingest learned them later
/// (e.g. the attachment carried a caption the upload did not). Only fills
/// empty slots — never overwrites an existing annotation.
///
/// # Errors
///
/// `Db` on query failure.
pub async fn backfill_annotations(
    pool: &SqlitePool,
    row: &MediaRow,
    caption: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    let new_caption = match (&row.caption, caption) {
        (None, Some(c)) if !c.trim().is_empty() => Some(c.to_owned()),
        _ => None,
    };
    let new_description = match (&row.description, description) {
        (None, Some(d)) if !d.trim().is_empty() => Some(d.to_owned()),
        _ => None,
    };
    if new_caption.is_none() && new_description.is_none() {
        return Ok(());
    }
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE media_catalog SET
            caption = COALESCE(caption, ?),
            description = COALESCE(description, ?),
            updated_at = ?
         WHERE catalog_id = ?",
    )
    .bind(new_caption)
    .bind(new_description)
    .bind(&now)
    .bind(row.catalog_id.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a `sqlx` error is a `SQLite` unique-constraint violation (the
/// catalog-id PK or the (sha256, owner) index).
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.message().contains("UNIQUE constraint failed"))
}

// ---------- minting ----------

/// Mint the next `c-<date>-<kind>-NNN.<ext>` for the day/kind pair by
/// scanning the existing prefix range. Race-free under the process-wide
/// single-writer guarantee (lockfile + WAL).
async fn mint_catalog_id(
    pool: &SqlitePool,
    date: &str,
    kind_: &str,
    ext: &str,
) -> Result<CatalogId> {
    let prefix = format!("c-{date}-{kind_}-");
    let pattern = format!("{prefix}%");
    let ids: Vec<(String,)> =
        sqlx::query_as("SELECT catalog_id FROM media_catalog WHERE catalog_id LIKE ?")
            .bind(&pattern)
            .fetch_all(pool)
            .await?;
    let max_nnn = ids
        .iter()
        .filter_map(|(id,)| {
            let tail = id.strip_prefix(&prefix)?;
            let (nnn, _ext) = tail.split_once('.')?;
            nnn.parse::<u64>().ok()
        })
        .max()
        .unwrap_or(0);
    let id = format!("{prefix}{:03}.{ext}", max_nnn + 1);
    CatalogId::parse(&id).map_err(|e| MediaError::Decode(e.to_string()))
}

/// Derive the catalog-id extension from the original filename when it has
/// a clean lowercase-alnum extension, else from the MIME type, else `bin`.
fn derive_ext(original_filename: Option<&str>, mime: &str) -> String {
    if let Some(name) = original_filename
        && let Some((_, ext)) = name.rsplit_once('.')
    {
        let ext = ext.to_ascii_lowercase();
        if !ext.is_empty()
            && ext.len() <= 8
            && ext
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return ext;
        }
    }
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/heic" => "heic",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/m4a" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        _ => "bin",
    }
    .to_owned()
}

// ---------- row decode ----------

const COLUMNS: &str = "catalog_id, sha256, kind, mime, size_bytes, owner_id, allow_ids, \
     sender_id, uploaded_by_consumer, caption, description, original_filename, \
     created_at, updated_at";

#[derive(sqlx::FromRow)]
struct RawMediaRow {
    catalog_id: String,
    sha256: String,
    kind: String,
    mime: String,
    size_bytes: i64,
    owner_id: String,
    allow_ids: Option<String>,
    sender_id: Option<String>,
    uploaded_by_consumer: Option<String>,
    caption: Option<String>,
    description: Option<String>,
    original_filename: Option<String>,
    created_at: String,
    updated_at: String,
}

fn decode_row(r: RawMediaRow) -> Result<MediaRow> {
    let catalog_id =
        CatalogId::parse(&r.catalog_id).map_err(|e| MediaError::Decode(e.to_string()))?;
    let owner_id = r
        .owner_id
        .parse::<Principal>()
        .map_err(|e| MediaError::Decode(e.to_string()))?;
    let allow_ids = match r.allow_ids.as_deref() {
        None | Some("") => Vec::new(),
        Some(s) => principals_from_json(s).map_err(MediaError::Decode)?,
    };
    let sender_id = r
        .sender_id
        .as_deref()
        .map(str::parse::<Principal>)
        .transpose()
        .map_err(|e| MediaError::Decode(e.to_string()))?;
    Ok(MediaRow {
        catalog_id,
        sha256: r.sha256,
        kind: r.kind,
        mime: r.mime,
        size_bytes: r.size_bytes,
        owner_id,
        allow_ids,
        sender_id,
        uploaded_by_consumer: r.uploaded_by_consumer,
        caption: r.caption,
        description: r.description,
        original_filename: r.original_filename,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        pool
    }

    fn sample_media(owner: &str) -> NewMedia {
        NewMedia {
            bytes: b"fake jpeg bytes".to_vec(),
            kind: kind::PHOTO.to_owned(),
            mime: "image/jpeg".to_owned(),
            owner: owner.parse().expect("principal"),
            uploaded_by_consumer: Some("sam".to_owned()),
            caption: Some("at the gate".to_owned()),
            description: None,
            original_filename: Some("IMG_0001.JPG".to_owned()),
        }
    }

    #[tokio::test]
    async fn store_writes_blob_then_row_and_mints_sequential_ids() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let one = store_media(&pool, dir.path(), sample_media("user:frodo"))
            .await
            .expect("store");
        assert!(!one.deduplicated);
        assert!(one.row.catalog_id.as_str().ends_with("-photo-001.jpg"));
        assert_eq!(one.row.kind, "photo");
        assert_eq!(one.row.mime, "image/jpeg");
        assert_eq!(one.row.owner_id, "user:frodo".parse().unwrap());
        assert!(one.row.allow_ids.is_empty());
        let blob = blob_path(dir.path(), &one.row.sha256);
        assert_eq!(std::fs::read(&blob).unwrap(), b"fake jpeg bytes");

        // Different bytes, same day/kind → NNN increments.
        let mut second = sample_media("user:frodo");
        second.bytes = b"different bytes".to_vec();
        let two = store_media(&pool, dir.path(), second).await.expect("store");
        assert!(two.row.catalog_id.as_str().ends_with("-photo-002.jpg"));
    }

    #[tokio::test]
    async fn same_bytes_same_owner_dedups_to_existing_row() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let first = store_media(&pool, dir.path(), sample_media("user:frodo"))
            .await
            .unwrap();
        let again = store_media(&pool, dir.path(), sample_media("user:frodo"))
            .await
            .unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.row.catalog_id, first.row.catalog_id);
    }

    #[tokio::test]
    async fn same_bytes_different_owner_mints_fresh_row_sharing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let frodo = store_media(&pool, dir.path(), sample_media("user:frodo"))
            .await
            .unwrap();
        let sam = store_media(&pool, dir.path(), sample_media("user:sam"))
            .await
            .unwrap();
        assert!(!sam.deduplicated);
        assert_ne!(sam.row.catalog_id, frodo.row.catalog_id);
        assert_eq!(sam.row.sha256, frodo.row.sha256);
    }

    #[tokio::test]
    async fn invalid_kind_and_empty_payload_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let mut italian = sample_media("user:frodo");
        italian.kind = "foto".to_owned();
        assert!(matches!(
            store_media(&pool, dir.path(), italian).await,
            Err(MediaError::InvalidKind(_))
        ));

        let mut empty = sample_media("user:frodo");
        empty.bytes = Vec::new();
        assert!(matches!(
            store_media(&pool, dir.path(), empty).await,
            Err(MediaError::EmptyPayload)
        ));
    }

    #[tokio::test]
    async fn acl_check_owner_reads_others_denied_until_widened() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let stored = store_media(&pool, dir.path(), sample_media("user:frodo"))
            .await
            .unwrap();
        let row = stored.row;
        assert!(row_visible_to(&row, "frodo", &[]));
        assert!(!row_visible_to(&row, "sam", &[]));
        assert!(!row_visible_to(&row, "gollum", &["famiglia".into()]));

        // A fact owned by frodo, allowing the famiglia group, embeds it.
        let grew = widen_acl(
            &pool,
            &row.catalog_id,
            &"user:frodo".parse().unwrap(),
            &["group:famiglia".parse().unwrap()],
            Some(&"user:sam".parse().unwrap()),
        )
        .await
        .unwrap();
        assert!(grew);

        let row = find_by_id(&pool, &row.catalog_id).await.unwrap().unwrap();
        assert!(row_visible_to(&row, "gollum", &["famiglia".into()]));
        assert!(row_visible_to(&row, "sam", &[]));
        assert!(!row_visible_to(&row, "saruman", &[]));

        // Widening is idempotent — same set again changes nothing.
        let grew_again = widen_acl(
            &pool,
            &row.catalog_id,
            &"user:frodo".parse().unwrap(),
            &["group:famiglia".parse().unwrap()],
            Some(&"user:sam".parse().unwrap()),
        )
        .await
        .unwrap();
        assert!(!grew_again);
    }

    #[tokio::test]
    async fn backfill_fills_empty_slots_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let pool = make_pool().await;

        let mut media = sample_media("user:frodo");
        media.caption = None;
        let stored = store_media(&pool, dir.path(), media).await.unwrap();

        backfill_annotations(&pool, &stored.row, Some("the gate"), Some("two hobbits"))
            .await
            .unwrap();
        let row = find_by_id(&pool, &stored.row.catalog_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.caption.as_deref(), Some("the gate"));
        assert_eq!(row.description.as_deref(), Some("two hobbits"));

        // Second backfill with different text must not overwrite.
        backfill_annotations(&pool, &row, Some("other"), None)
            .await
            .unwrap();
        let row = find_by_id(&pool, &row.catalog_id).await.unwrap().unwrap();
        assert_eq!(row.caption.as_deref(), Some("the gate"));
    }

    #[test]
    fn ext_derivation_prefers_clean_filename_then_mime() {
        assert_eq!(derive_ext(Some("IMG_0001.JPG"), "image/png"), "jpg");
        assert_eq!(derive_ext(Some("noext"), "image/png"), "png");
        assert_eq!(derive_ext(Some("weird.TAR.GZ"), "audio/mpeg"), "gz");
        assert_eq!(derive_ext(None, "application/pdf"), "pdf");
        assert_eq!(derive_ext(None, "application/x-mystery"), "bin");
    }

    #[test]
    fn inline_safe_rejects_scriptable_types_even_with_mime_parameters() {
        assert!(inline_safe("image/jpeg"));
        assert!(inline_safe("image/png; some=param"));
        assert!(inline_safe("application/pdf"));
        assert!(inline_safe("VIDEO/MP4"));
        // The stored-XSS family fails closed in every spelling.
        assert!(!inline_safe("image/svg+xml"));
        assert!(!inline_safe("image/svg+xml; charset=utf-8"));
        assert!(!inline_safe("image/svg+xml;charset=utf-8"));
        assert!(!inline_safe("IMAGE/SVG+XML"));
        assert!(!inline_safe("text/html"));
        assert!(!inline_safe("text/html; charset=utf-8"));
        assert!(!inline_safe(""));
    }

    #[test]
    fn kind_vocabulary_is_closed() {
        assert!(kind::is_canonical("photo"));
        assert!(kind::is_canonical("doc"));
        assert!(!kind::is_canonical("foto"));
        assert!(!kind::is_canonical(""));
    }
}
