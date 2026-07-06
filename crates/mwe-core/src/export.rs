// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wiki subtree export — the engine DB joined with the on-disk prose
//! into a portable, self-describing archive.
//!
//! The runtime marker is the bare region key (`{{f=uuid}}` — see
//! marker grammar); the
//! per-fact ACL lives in the `fact_index` columns. An exported page must
//! stand alone *without* the engine DB next to it, so export rewrites
//! every DB-known region to the **full marker form**
//! (`{{owner=… allow=… sender=… f=…}}body{{/}}`) via
//! [`crate::capture::render_full_marker`]. Because the parser accepts the
//! full form as input forever, an exported archive re-imports losslessly.
//!
//! Scope and policy:
//!
//! - **The exported body is the on-disk prose span** (the render), not
//!   `fact_index.text` — export is the prose made portable, not a DB dump.
//! - A region whose fact key has **no DB row** (or no key at all) is left
//!   verbatim and counted in the [`ExportReport`]. Importers resolve such
//!   regions at the destination wiki's `acl_default` — fail-closed on
//!   both sides.
//! - `_meta.md` travels verbatim; `_captures.md` is **excluded**
//!   (unpromoted buffer state is engine-internal, not interchange).
//! - **Referenced media travels too**: every `{{embed=…}}` in the
//!   exported pages (standalone or inside a region body) pulls its
//!   blob from the content-addressed store into `_media/<catalog_id>`
//!   (the id carries the extension, so the files open naturally), and
//!   `_media/_catalog.json` carries each item's catalog row — kind,
//!   MIME, sha256 and the per-media ACL — the media analogue of the
//!   full marker, since an embed marker has no inline-attribute form
//!   (media pipeline).
//!   A dangling embed (no catalog row, or a row whose blob is gone) is
//!   counted in the report, never silently dropped.
//! - Packaging is a plain uncompressed `tar` (`application/x-tar`):
//!   memory-wiki prose is small and a single dependency-free format
//!   keeps the archive scriptable. Entries are deterministic (walk and
//!   page enumeration are sorted; media sorted by catalog id; mtime
//!   pinned to the epoch).

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::acl::FactAclMap;
use crate::capture::render_full_marker;
use crate::fact_index;
use crate::media;
use crate::parser::{ParseEvent, collect_embeds, parse};
use crate::types::{CatalogId, WikiId};
use crate::wiki::{
    CAPTURES_FILENAME, META_FILENAME, WikiError, WikiTree, list_wiki_pages,
    workdir_relative_source_path,
};

/// Errors surfaced by [`export_wiki_subtree`].
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Tree walk / page read failure.
    #[error("export wiki: {0}")]
    Wiki(#[from] WikiError),

    /// `fact_index` ACL-map load failure.
    #[error("export fact_index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),

    /// `media_catalog` lookup failure while bundling referenced media.
    #[error("export media_catalog: {0}")]
    Media(#[from] media::MediaError),

    /// Manifest serialization failure.
    #[error("export media manifest: {0}")]
    Json(#[from] serde_json::Error),

    /// Archive write failure (in-memory tar builder).
    #[error("export archive: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ExportError>;

/// Counters of one export run, surfaced so silent degradation is
/// impossible.
///
/// A non-zero `regions_unindexed` means some prose travels without its
/// own ACL and will fall back to the destination's `acl_default` on
/// import.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    /// Wikis included (the root + its descendants).
    pub wikis: usize,
    /// Pages written to the archive (`_meta.md` files included).
    pub pages: usize,
    /// Regions rewritten to the full self-describing marker form.
    pub regions_rewritten: usize,
    /// Regions left verbatim because the engine DB does not know their
    /// fact key (or the marker carries no key at all).
    pub regions_unindexed: usize,
    /// Distinct referenced media items bundled under `_media/`.
    pub media_bundled: usize,
    /// Referenced media that could NOT travel: no catalog row for the
    /// embed key, or the row's blob is missing from the store.
    pub media_missing: usize,
}

/// One finished export: the archive bytes plus the run report.
#[derive(Debug)]
pub struct WikiExport {
    /// Complete uncompressed tar stream, ready to serve as
    /// `application/x-tar`.
    pub tar_bytes: Vec<u8>,
    /// Name of the archive's top-level directory (the exported wiki's
    /// directory name) — handy for download filenames.
    pub root_dir: String,
    /// Counters of the run.
    pub report: ExportReport,
}

/// Export the wiki `wiki_id` **and every descendant wiki** as a portable
/// full-marker tar archive.
///
/// Walks the subtree depth-first (deterministic order), loads each
/// page's authoritative fact-key → ACL map from the engine DB
/// ([`fact_index::page_acl_map`]) and rewrites the page's regions to the
/// full marker form. See the module docs for the exact travel policy.
///
/// # Errors
///
/// [`ExportError::Wiki`] when `wiki_id` does not exist or a page read
/// fails; [`ExportError::FactIndex`] / [`ExportError::Io`] as labelled.
pub async fn export_wiki_subtree(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &WikiId,
) -> Result<WikiExport> {
    let root = tree.locate(wiki_id)?;
    let root_rel: PathBuf = root.rel_dir().to_path_buf();
    // Archive entries are rooted at the exported wiki's directory name:
    // exporting `wikis/alice` yields `alice/index.md`, …; exporting the
    // nested `wikis/alice/garden` yields `garden/…`.
    let strip_base = root_rel
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    let root_dir = root_rel.file_name().map_or_else(
        || wiki_id.as_str().to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );

    let mut builder = tar::Builder::new(Vec::new());
    let mut report = ExportReport::default();
    // Distinct embed keys across every exported page — BTreeSet keeps
    // the media entries deterministic.
    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for d in tree.walk()? {
        if !d.rel_dir.starts_with(&root_rel) {
            continue;
        }
        report.wikis += 1;
        let entry_dir = d
            .rel_dir
            .strip_prefix(&strip_base)
            .unwrap_or(&d.rel_dir)
            .to_path_buf();

        // `_meta.md` verbatim — identity, family, sharing, acl_default.
        let meta_raw = std::fs::read(d.abs_dir.join(META_FILENAME))?;
        append_entry(&mut builder, &entry_dir.join(META_FILENAME), &meta_raw)?;
        report.pages += 1;

        // Pages of this wiki only (sub-wikis come up as their own walk
        // nodes); `list_wiki_pages` already excludes `_meta.md` and
        // `_captures.md`.
        for page in list_wiki_pages(&d.abs_dir)? {
            debug_assert!(
                page.rel_path
                    .file_name()
                    .is_none_or(|n| n != CAPTURES_FILENAME),
                "page enumeration must never surface the capture buffer"
            );
            let raw = std::fs::read_to_string(&page.abs_path)?;
            let source_path = workdir_relative_source_path(tree.workdir(), &page.abs_path);
            let acl_map = fact_index::page_acl_map(pool, &source_path).await?;
            let (rewritten, rewritten_n, unindexed_n) = rewrite_page_markers(&raw, &acl_map);
            report.regions_rewritten += rewritten_n;
            report.regions_unindexed += unindexed_n;
            // Standalone embeds AND embeds inside region bodies — the
            // shared enumeration, so fact-carried media is never missed.
            for cid in collect_embeds(&raw) {
                referenced.insert(cid.as_str().to_owned());
            }
            append_entry(
                &mut builder,
                &entry_dir.join(&page.rel_path),
                rewritten.as_bytes(),
            )?;
            report.pages += 1;
        }
    }

    append_referenced_media(
        pool,
        tree.workdir(),
        &mut builder,
        &root_dir,
        &referenced,
        &mut report,
    )
    .await?;

    let tar_bytes = builder.into_inner()?;
    Ok(WikiExport {
        tar_bytes,
        root_dir,
        report,
    })
}

/// One `_media/_catalog.json` manifest entry — the catalog row made
/// portable (the media analogue of the full marker: an embed key has no
/// inline-attribute ACL form, so the manifest carries it).
#[derive(serde::Serialize)]
struct MediaManifestEntry {
    catalog_id: String,
    sha256: String,
    kind: String,
    mime: String,
    size_bytes: i64,
    owner: String,
    allow: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// Bundle every referenced media blob under `<root_dir>/_media/` plus
/// the `_catalog.json` manifest. Dangling references (no row, or a row
/// whose blob is gone) count as `media_missing` — surfaced, not fatal:
/// the prose archive is still worth having.
async fn append_referenced_media(
    pool: &SqlitePool,
    workdir: &Path,
    builder: &mut tar::Builder<Vec<u8>>,
    root_dir: &str,
    referenced: &std::collections::BTreeSet<String>,
    report: &mut ExportReport,
) -> Result<()> {
    if referenced.is_empty() {
        return Ok(());
    }
    let media_dir = Path::new(root_dir).join("_media");
    let mut manifest: Vec<MediaManifestEntry> = Vec::new();
    for id in referenced {
        // Keys come from `collect_embeds`, so they re-parse by
        // construction; guard anyway.
        let Ok(catalog_id) = CatalogId::parse(id) else {
            report.media_missing += 1;
            continue;
        };
        let Some(row) = media::find_by_id(pool, &catalog_id).await? else {
            report.media_missing += 1;
            continue;
        };
        let blob_abs = media::blob_path(workdir, &row.sha256);
        let bytes = match std::fs::read(&blob_abs) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(catalog_id = %catalog_id, error = %e, "export: referenced blob missing");
                report.media_missing += 1;
                continue;
            },
        };
        append_entry(builder, &media_dir.join(id), &bytes)?;
        manifest.push(MediaManifestEntry {
            catalog_id: id.clone(),
            sha256: row.sha256,
            kind: row.kind,
            mime: row.mime,
            size_bytes: row.size_bytes,
            owner: row.owner_id.to_string(),
            allow: row.allow_ids.iter().map(ToString::to_string).collect(),
            sender: row.sender_id.as_ref().map(ToString::to_string),
            caption: row.caption,
            description: row.description,
        });
        report.media_bundled += 1;
    }
    if !manifest.is_empty() {
        let json = serde_json::to_vec_pretty(&manifest)?;
        append_entry(builder, &media_dir.join("_catalog.json"), &json)?;
    }
    Ok(())
}

/// Rewrite every DB-known region of `raw` to the full marker form,
/// leaving everything else (prose, embeds, unindexed regions, malformed
/// fragments) byte-for-byte intact. Returns the rewritten page plus the
/// (rewritten, unindexed) region counts.
fn rewrite_page_markers(raw: &str, acl_map: &FactAclMap) -> (String, usize, usize) {
    let parsed = parse(raw);
    let mut out = String::with_capacity(raw.len() + raw.len() / 4);
    let mut last = 0;
    let mut rewritten = 0;
    let mut unindexed = 0;
    for ev in &parsed.events {
        let ParseEvent::Region {
            start,
            end,
            attrs,
            body_start,
            body_end,
            ..
        } = ev
        else {
            continue;
        };
        let Some(acl) = attrs.fact_id.as_ref().and_then(|f| acl_map.get(f)) else {
            unindexed += 1;
            continue;
        };
        // `attrs.fact_id` is `Some` here — `acl_map.get` matched on it.
        let fact_id = attrs.fact_id.as_ref().expect("checked above");
        out.push_str(&raw[last..*start]);
        out.push_str(&render_full_marker(
            fact_id,
            &acl.owner,
            &acl.allow,
            acl.sender.as_ref(),
            &raw[*body_start..*body_end],
        ));
        last = *end;
        rewritten += 1;
    }
    out.push_str(&raw[last..]);
    (out, rewritten, unindexed)
}

/// Append one file to the in-memory tar with deterministic metadata
/// (mode 0644, epoch mtime) so the same tree always produces the same
/// bytes.
fn append_entry(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder.append_data(&mut header, path, bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read as _;
    use std::path::PathBuf;
    use std::sync::Arc;

    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
    use crate::embedder::{Embedder, FakeEmbedder};
    use crate::types::{FactId, Principal};
    use crate::wiki::atomic_write;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
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

    fn write_user_wiki_meta(abs_dir: &Path, wiki_id: &str) {
        std::fs::create_dir_all(abs_dir).unwrap();
        let slug = wiki_id.rsplit('-').next().unwrap_or(wiki_id);
        let meta = format!(
            "---\nwiki_id: {wiki_id}\ntitle: {slug}\nwiki_type: wiki-user\nslug: {slug}\n\
             parent_wiki_id: null\nacl_default: user:alice\n---\n",
        );
        atomic_write(&abs_dir.join(META_FILENAME), meta.as_bytes()).expect("write meta");
    }

    async fn capture_fact(
        pool: &SqlitePool,
        tree: &WikiTree,
        wiki_id: &str,
        page: &str,
        body: &str,
        allow: Vec<Principal>,
        sender: Option<Principal>,
    ) -> FactId {
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki_id).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
            allow,
            sender,
            fact_type: None,
            topics: vec![],
            dedup_threshold: Some(1.01),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let outcome = wiki_capture(tree, pool, embedder, req).await.unwrap();
        match outcome.action {
            CaptureAction::Captured { .. } => outcome.fact_id,
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    /// Untar the archive into a path → UTF-8 content map.
    fn untar(bytes: &[u8]) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut archive = tar::Archive::new(bytes);
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("path").to_string_lossy().into_owned();
            let mut content = String::new();
            entry.read_to_string(&mut content).expect("utf-8 content");
            out.insert(path, content);
        }
        out
    }

    /// Fact ids minted by [`seed_subtree_fixture`].
    struct SubtreeFixture {
        plain: FactId,
        acl: FactId,
        garden: FactId,
    }

    /// The unindexed bare region appended by hand in the fixture.
    const ORPHAN_KEY: &str = "018f1234-5678-7abc-9def-0123456789ab";

    /// Seed `alice` (+ nested `garden`, + sibling `bob`) with captured
    /// facts, one hand-appended unindexed region on `alice/index.md`,
    /// and a capture-buffer file that must never travel.
    async fn seed_subtree_fixture(
        dir: &Path,
        tree: &WikiTree,
        pool: &SqlitePool,
    ) -> SubtreeFixture {
        write_user_wiki_meta(&dir.join("wikis/alice"), "alice");
        write_user_wiki_meta(&dir.join("wikis/alice/garden"), "alice-garden");
        write_user_wiki_meta(&dir.join("wikis/bob"), "bob");

        let f_plain = capture_fact(
            pool,
            tree,
            "alice",
            "index.md",
            "Alice likes tea",
            vec![],
            None,
        )
        .await;
        let f_acl = capture_fact(
            pool,
            tree,
            "alice",
            "index.md",
            "Shared secret with Bob",
            vec!["user:bob".parse().unwrap()],
            Some("user:carol".parse().unwrap()),
        )
        .await;
        let f_garden = capture_fact(
            pool,
            tree,
            "alice-garden",
            "notes.md",
            "Tomatoes in June",
            vec![],
            None,
        )
        .await;
        capture_fact(
            pool,
            tree,
            "bob",
            "index.md",
            "Bob's own fact",
            vec![],
            None,
        )
        .await;

        // An unindexed bare region: appended by hand, never captured.
        let index_path = dir.join("wikis/alice/index.md");
        let mut index_raw = std::fs::read_to_string(&index_path).unwrap();
        index_raw.push_str("{{f=");
        index_raw.push_str(ORPHAN_KEY);
        index_raw.push_str("}}mystery prose{{/}}\n");
        std::fs::write(&index_path, index_raw).unwrap();

        // Capture-buffer state must not travel.
        std::fs::write(dir.join("wikis/alice/_captures.md"), "buffered\n").unwrap();

        SubtreeFixture {
            plain: f_plain,
            acl: f_acl,
            garden: f_garden,
        }
    }

    /// The core round-trip: captured facts export as full markers
    /// carrying the DB ACL; an unindexed region stays bare; the capture
    /// buffer never travels; sibling wikis stay out; descendant wikis
    /// come along.
    #[tokio::test]
    async fn export_round_trips_db_acl_into_full_markers() {
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let fx = seed_subtree_fixture(dir.path(), &tree, &pool).await;

        let export = export_wiki_subtree(&pool, &tree, &WikiId::parse("alice").unwrap())
            .await
            .expect("export");
        assert_eq!(export.root_dir, "alice");
        let entries = untar(&export.tar_bytes);

        // Layout: root + descendant travel, sibling and buffer do not.
        assert!(entries.contains_key("alice/_meta.md"), "{entries:?}");
        assert!(entries.contains_key("alice/index.md"), "{entries:?}");
        assert!(entries.contains_key("alice/garden/_meta.md"), "{entries:?}");
        assert!(entries.contains_key("alice/garden/notes.md"), "{entries:?}");
        assert!(
            entries.keys().all(|k| !k.contains(CAPTURES_FILENAME)),
            "capture buffer must be excluded: {entries:?}"
        );
        assert!(
            entries.keys().all(|k| k.starts_with("alice/")),
            "sibling wikis must stay out: {entries:?}"
        );

        // `_meta.md` travels verbatim.
        let meta_on_disk =
            std::fs::read_to_string(dir.path().join("wikis/alice/_meta.md")).unwrap();
        assert_eq!(entries["alice/_meta.md"], meta_on_disk);

        // Every DB-known region was rewritten to a full marker carrying
        // the DB ACL; the orphan region stayed bare.
        let exported_index = &entries["alice/index.md"];
        let parsed = parse(exported_index);
        let mut seen = BTreeMap::new();
        for ev in &parsed.events {
            if let ParseEvent::Region { attrs, body, .. } = ev {
                seen.insert(
                    attrs
                        .fact_id
                        .as_ref()
                        .expect("region keeps its key")
                        .to_string(),
                    (attrs.clone(), body.clone()),
                );
            }
        }
        let (plain_attrs, plain_body) = &seen[fx.plain.as_str()];
        assert_eq!(plain_attrs.acl.owner, Some("user:alice".parse().unwrap()));
        assert!(plain_attrs.acl.allow.is_empty());
        assert_eq!(
            plain_attrs.sender,
            Some("user:alice".parse().unwrap()),
            "a self-fact's sender is materialized to the owner and exported explicitly (never collapsed)"
        );
        assert_eq!(plain_body, "Alice likes tea");

        let (acl_attrs, acl_body) = &seen[fx.acl.as_str()];
        assert_eq!(acl_attrs.acl.owner, Some("user:alice".parse().unwrap()));
        assert_eq!(
            acl_attrs.acl.allow,
            vec!["user:bob".parse::<Principal>().unwrap()]
        );
        assert_eq!(acl_attrs.sender, Some("user:carol".parse().unwrap()));
        assert_eq!(acl_body, "Shared secret with Bob");

        let (orphan_attrs, orphan_body) = &seen[ORPHAN_KEY];
        assert_eq!(
            orphan_attrs.acl.owner, None,
            "unindexed region must stay bare"
        );
        assert!(orphan_attrs.acl.allow.is_empty());
        assert_eq!(orphan_body, "mystery prose");

        // Descendant page rewritten too.
        let garden = &entries["alice/garden/notes.md"];
        assert!(
            garden.contains("owner=user:alice") && garden.contains(fx.garden.as_str()),
            "{garden}"
        );

        // Report: 2 wikis, 4 files, 3 rewritten regions, 1 unindexed.
        assert_eq!(export.report.wikis, 2);
        assert_eq!(export.report.pages, 4);
        assert_eq!(export.report.regions_rewritten, 3);
        assert_eq!(export.report.regions_unindexed, 1);
    }

    /// Exporting a nested wiki roots the archive at the nested
    /// directory and leaves the ancestor out.
    #[tokio::test]
    async fn export_of_nested_wiki_roots_at_its_own_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;

        write_user_wiki_meta(&dir.path().join("wikis/alice"), "alice");
        write_user_wiki_meta(&dir.path().join("wikis/alice/garden"), "alice-garden");
        capture_fact(&pool, &tree, "alice", "index.md", "Root fact", vec![], None).await;
        capture_fact(
            &pool,
            &tree,
            "alice-garden",
            "notes.md",
            "Nested fact",
            vec![],
            None,
        )
        .await;

        let export = export_wiki_subtree(&pool, &tree, &WikiId::parse("alice-garden").unwrap())
            .await
            .expect("export");
        assert_eq!(export.root_dir, "garden");
        let entries = untar(&export.tar_bytes);
        assert!(entries.contains_key("garden/_meta.md"), "{entries:?}");
        assert!(entries.contains_key("garden/notes.md"), "{entries:?}");
        assert!(
            entries.keys().all(|k| k.starts_with("garden/")),
            "ancestor pages must stay out: {entries:?}"
        );
        assert_eq!(export.report.wikis, 1);
    }

    /// Referenced media travels: the blob lands at `_media/<id>`, the
    /// manifest carries the catalog row (ACL included), and a dangling
    /// embed counts as missing instead of silently vanishing. Blob
    /// bytes are ASCII on purpose — the `untar` helper reads UTF-8.
    #[tokio::test]
    async fn export_bundles_referenced_media_with_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        write_user_wiki_meta(&dir.path().join("wikis/alice"), "alice");

        let stored = crate::media::store_media(
            &pool,
            dir.path(),
            crate::media::NewMedia {
                bytes: b"jpegbytes".to_vec(),
                kind: crate::media::kind::PHOTO.to_owned(),
                mime: "image/jpeg".to_owned(),
                owner: "user:alice".parse().unwrap(),
                uploaded_by_consumer: None,
                caption: Some("at the gate".to_owned()),
                description: None,
                original_filename: Some("photo.jpg".to_owned()),
            },
        )
        .await
        .expect("store media");
        let cid = stored.row.catalog_id;
        crate::media::widen_acl(
            &pool,
            &cid,
            &"user:alice".parse().unwrap(),
            &["group:famiglia".parse().unwrap()],
            None,
        )
        .await
        .expect("widen");

        // A fact whose body carries the embed (in-region), plus a
        // dangling embed in free prose.
        capture_fact(
            &pool,
            &tree,
            "alice",
            "index.md",
            &format!("Photo of the gate {{{{embed={cid}}}}}"),
            vec![],
            None,
        )
        .await;
        let index_path = dir.path().join("wikis/alice/index.md");
        let mut raw = std::fs::read_to_string(&index_path).unwrap();
        raw.push_str("\n{{embed=c-2020-01-01-photo-001.jpg}}\n");
        std::fs::write(&index_path, raw).unwrap();

        let export = export_wiki_subtree(&pool, &tree, &WikiId::parse("alice").unwrap())
            .await
            .expect("export");
        let entries = untar(&export.tar_bytes);

        let blob_key = format!("alice/_media/{cid}");
        assert_eq!(entries[&blob_key], "jpegbytes", "{entries:?}");
        let manifest: serde_json::Value =
            serde_json::from_str(&entries["alice/_media/_catalog.json"]).expect("manifest json");
        let items = manifest.as_array().expect("array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["catalog_id"], cid.as_str());
        assert_eq!(items[0]["kind"], "photo");
        assert_eq!(items[0]["owner"], "user:alice");
        assert_eq!(items[0]["allow"][0], "group:famiglia");
        assert_eq!(items[0]["caption"], "at the gate");

        assert_eq!(export.report.media_bundled, 1);
        assert_eq!(export.report.media_missing, 1, "the dangling embed counts");
    }

    /// Unknown wiki id surfaces as a wiki error, not a panic or an
    /// empty archive.
    #[tokio::test]
    async fn export_unknown_wiki_errors() {
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let err = export_wiki_subtree(&pool, &tree, &WikiId::parse("ghost").unwrap())
            .await
            .expect_err("must fail");
        assert!(matches!(
            err,
            ExportError::Wiki(WikiError::WikiNotFound { .. })
        ));
    }
}
