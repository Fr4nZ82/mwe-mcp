// SPDX-License-Identifier: AGPL-3.0-or-later
//! Filesystem watcher → `fact_index` reindex pipeline.
//!
//! When a third-party editor (Obsidian, the operator with a text
//! editor) touches a markdown file under `<workdir>/wikis/**`, this
//! module re-parses it and reconciles the `fact_index`. What
//! "reconcile" means depends on the wiki's family:
//!
//! - **Smart wikis** (smart-consumer project wikis, written verbatim
//!   via `wiki_admin_*` or a direct filesystem edit): plain markdown
//!   with no per-fragment `{{f=…}}` markers, so recall indexes the
//!   **content** — each page is chunked into heading-delimited sections,
//!   embedded, and the page's `fact_index` rows are drop-and-reinserted.
//!   Every row carries the wiki-level ACL from `_meta` (owner +
//!   `shared_with`) projected onto it; a removed page's rows are
//!   hard-dropped (no tombstone). Unchanged sections reuse their stored
//!   embedding so an idle page mutates zero rows.
//! - **Standard wikis** (compiler/capture output): the DB is the
//!   authoritative fact store and pages are its prose render, so the
//!   sweep shrinks to **offset-and-existence repair** — region offsets
//!   are repaired after hand edits, a hand-deleted marker or page still
//!   tombstones its *rendered* rows (the operator's forget gesture),
//!   but rows are never created or rewritten from disk markers, and
//!   offset-less rows (pending renders) are never touched.
//!
//! ## Two entry points
//!
//! - [`reindex_file`] re-syncs a single file (canonical fast path for
//!   watcher events).
//! - [`reindex_full`] walks every wiki and re-syncs every page under it
//!   (safety net for missed events, e.g. NFS / large rename batches).
//!
//! Both are idempotent: running the same pipeline twice in a row over
//! an unchanged tree mutates zero rows the second time.
//!
//! ## Watcher glue
//!
//! [`run_watcher_loop`] consumes a [`watcher::WatchedChange`] stream and
//! routes every event through [`reindex_file`]; it is the canonical way
//! to wire the `notify`-backed [`watcher::WikiWatcher`] to the DB.
//!
//! ## Safety net
//!
//! [`run_safety_net_loop`] re-runs [`reindex_full`] on a configurable
//! interval (default 5 minutes per the
//! reindex pipeline) so a
//! missed event never permanently de-syncs the index.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::embedder::{Embedder, EmbedderError};
use crate::fact_index::{self, FactIndexRow};
use crate::parser::{self, ParseEvent};
use crate::sections;
use crate::types::FactId;
use crate::watcher::WatchedChange;
use crate::wiki::{DiscoveredWiki, WikiError, WikiTree};

/// Default cadence of [`run_safety_net_loop`] — the 5-minute window.
///
/// The reindex pipeline calls it out as the
/// REM-level full re-scan that catches anything the watcher misses.
pub const SAFETY_NET_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Errors raised by the reindex pipeline.
#[derive(Debug, Error)]
pub enum ReindexError {
    /// Underlying filesystem error reading a page.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Failure walking / locating wikis.
    #[error("wiki: {0}")]
    Wiki(#[from] WikiError),
    /// DB-side failure on insert / update / tombstone.
    #[error("fact_index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),
    /// DB-side failure on the smart-wiki section index.
    #[error("wiki_sections: {0}")]
    Sections(#[from] sections::SectionError),
    /// Embedder failure when re-embedding a changed body.
    #[error("embedder: {0}")]
    Embedder(#[from] EmbedderError),
    /// The given path is not under the watched wikis tree, so we cannot
    /// resolve it to a `(wiki_id, source_path)` pair.
    #[error("path {0} is not inside the watched wikis tree")]
    PathOutsideTree(PathBuf),
}

/// Result alias for the reindex pipeline.
pub type Result<T> = std::result::Result<T, ReindexError>;

// ---------- Embedder-identity guard (roadmap 18g) ----------

/// `engine_meta` key: the model id the store's vectors were built with.
const META_EMBEDDER_MODEL: &str = "embedder_model_id";
/// `engine_meta` key: the vector dimension the store's vectors were built with.
const META_EMBEDDER_DIM: &str = "embedder_dim";

/// Outcome of [`check_embedder_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedderIdentity {
    /// No identity was recorded (a fresh store, or one upgraded from before
    /// this guard with a consistent dimension); the configured embedder was
    /// just stamped as the store's identity.
    Stamped,
    /// The configured embedder matches the recorded identity.
    Match,
    /// The configured embedder differs from what the store was built with —
    /// similarity search is wrong until a full reindex re-embeds every fact.
    /// Never silently overwritten; the operator must reindex.
    Mismatch {
        /// Model id the store was built with (`"(unknown)"` when only the
        /// dimension could be recovered, from the rows themselves).
        stored_model: String,
        /// Vector dimension the store was built with.
        stored_dim: usize,
        /// Model id the configured embedder produces.
        configured_model: String,
        /// Vector dimension the configured embedder produces.
        configured_dim: usize,
    },
}

/// Compare the configured embedder against the identity the store's vectors
/// were built with (roadmap 18g), so swapping the embedding model is caught
/// instead of silently corrupting cosine similarity.
///
/// On a store with no recorded identity (fresh, or upgraded from before the
/// guard) the configured embedder is stamped and [`EmbedderIdentity::Stamped`]
/// returned — *unless* the index already holds vectors of a different
/// dimension, in which case the (dangerous, cosine-breaking) mismatch is
/// reported rather than stamped over. A real mismatch is never overwritten:
/// the remedy is a full reindex that re-embeds every fact.
///
/// # Errors
///
/// Any DB failure reading / writing `engine_meta` or sampling `fact_index`.
pub async fn check_embedder_identity(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
) -> anyhow::Result<EmbedderIdentity> {
    let configured_model = embedder.model_id().to_owned();
    let configured_dim = embedder.dimensions();

    let stored_model = crate::db::meta_get(pool, META_EMBEDDER_MODEL).await?;
    let stored_dim = crate::db::meta_get(pool, META_EMBEDDER_DIM).await?;

    if let (Some(model), Some(dim)) = (stored_model, stored_dim) {
        let dim: usize = dim.parse().unwrap_or(0);
        if model == configured_model && dim == configured_dim {
            return Ok(EmbedderIdentity::Match);
        }
        return Ok(EmbedderIdentity::Mismatch {
            stored_model: model,
            stored_dim: dim,
            configured_model,
            configured_dim,
        });
    }

    // No recorded identity. Guard against a store that already holds vectors
    // of a different dimension (an upgrade from before this guard, or a model
    // swapped before the first stamp): the dimension mismatch breaks cosine
    // outright, so report it rather than stamping over it.
    if let Some(actual_dim) = fact_index::sample_embedding_dim(pool).await?
        && actual_dim != configured_dim
    {
        return Ok(EmbedderIdentity::Mismatch {
            stored_model: "(unknown)".to_owned(),
            stored_dim: actual_dim,
            configured_model,
            configured_dim,
        });
    }

    // Fresh (or consistent-dimension) store: stamp the configured embedder.
    crate::db::meta_set(pool, META_EMBEDDER_MODEL, &configured_model).await?;
    crate::db::meta_set(pool, META_EMBEDDER_DIM, &configured_dim.to_string()).await?;
    Ok(EmbedderIdentity::Stamped)
}

/// Tombstone reason used when a marker disappears from a page that
/// still exists on disk.
pub const REASON_MARKER_REMOVED: &str = "filesystem_marker_removed";

/// Tombstone reason used when the entire file is deleted from disk.
pub const REASON_FILE_REMOVED: &str = "filesystem_file_removed";

// ---------- Per-file report ----------

/// Summary of what [`reindex_file`] did to a single source path.
///
/// Returned even when nothing changed (every counter is zero in that
/// case) — the caller can branch on `inserted + updated + orphaned > 0`
/// to decide whether to log at info level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexFileReport {
    /// Workdir-relative source path the report describes.
    pub source_path: String,
    /// Wiki id the source path resolved to. `None` when the file lives
    /// outside any wiki (e.g. `_styles/` or a stray non-wiki markdown).
    pub wiki_id: Option<String>,
    /// Rows freshly inserted — smart-wiki section rows reinserted on a
    /// content change; standard-wiki rows are never created from disk.
    pub inserted: usize,
    /// Rows updated in place: a standard-wiki offsets-only repair (the
    /// ACL columns and the canonical claim text are DB-authoritative and
    /// never updated from disk).
    pub updated: usize,
    /// Rows removed: a standard-wiki marker/page that disappeared from
    /// disk (tombstoned), or a smart-wiki page's stale section rows
    /// hard-dropped before reinsert.
    pub orphaned: usize,
    /// Whether the file itself was missing from disk on scan.
    pub file_missing: bool,
    /// Non-fatal warnings encountered (parser warnings, principal parse
    /// failures, …). Empty on the happy path.
    pub warnings: Vec<String>,
}

impl ReindexFileReport {
    /// Whether the file actually changed compared to the index.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.inserted + self.updated + self.orphaned > 0
    }
}

/// Summary of what [`reindex_full`] did across the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexFullReport {
    /// Total markdown files visited.
    pub files_scanned: usize,
    /// Total fresh rows across the full scan.
    pub total_inserted: usize,
    /// Total in-place updates.
    pub total_updated: usize,
    /// Total tombstones.
    pub total_orphaned: usize,
    /// Per-file reports for files that actually changed (idle files are
    /// elided to keep the report small).
    pub per_file: Vec<ReindexFileReport>,
}

// ---------- reindex_file ----------

/// Re-sync a single file against the `fact_index`.
///
/// Reads the file at `abs_path` and applies the family's reconciliation
/// (see the module docs):
///
/// **Smart** (content-indexed, markerless — [`section_index_page`]):
/// - the page is chunked into heading-delimited sections, each embedded,
///   and the page's `fact_index` rows are drop-and-reinserted; every row
///   carries the wiki-level ACL from `_meta` (owner + `shared_with`)
/// - an unchanged page mutates zero rows; an edit re-embeds only the
///   changed sections (unchanged section text reuses its stored vector)
/// - file missing from disk → the page's rows are hard-dropped (no
///   tombstone)
///
/// **Standard** (DB-authoritative; pages are renders — diffs the disk
/// markers against the active rows for the same `source_path`):
/// - markers with no row are left alone (stale render residue — the
///   next compile rewrites the page; facts are never authored by
///   editing standard-wiki prose)
/// - markers with a row → offsets repaired via `fact_index::move_region`
///   when drifted; the row's canonical claim `text` is never overwritten
/// - the orphan / file-missing tombstones apply only to rows **with**
///   region offsets — an offset-less row is a pending render (capture
///   crash window, comment-channel add), not an orphan
/// - markers without an `f=<UUIDv7>` attribute are ignored on purpose:
///   they are region-level ACL wrappers, not indexable facts
///
/// # Errors
///
/// Surfaces [`ReindexError`] on filesystem / DB / embedder failure.
/// Parser warnings are accumulated into the report, never raised.
pub async fn reindex_file(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    abs_path: &Path,
) -> Result<ReindexFileReport> {
    // Reserved underscore-pages are not indexable content: `_captures.md`
    // (rebuilt by `capture_buffer::reindex_capture_journal`), `_meta.md`
    // (wiki config), and `_briefing.md` / `_briefing.archive.md` (the smart
    // consumer's feedback INBOX, not knowledge it authored). The standard
    // sweep ignores them for lack of markers; the smart section-indexer
    // must never turn their bytes into recallable fact rows.
    if is_reserved_page(abs_path) {
        return Ok(ReindexFileReport::default());
    }
    let source_path = relative_source_path(tree, abs_path)?;
    let resolved_wiki = resolve_wiki_for_path(tree, abs_path)?;
    // Narrative rows without region offsets are pending renders (the
    // committed fact exists, its prose does not yet) — the existence
    // sweep must spare them. Companion rows are filesystem-authored, so
    // every row is expected on disk; same for the odd row whose path
    // resolves to no wiki at all.
    let spare_pending = matches!(&resolved_wiki, Some(r) if !r.smart);
    let mut report = ReindexFileReport {
        source_path: source_path.clone(),
        wiki_id: resolved_wiki.as_ref().map(|r| r.wiki_id.clone()),
        ..Default::default()
    };

    let raw = match std::fs::read_to_string(abs_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            report.file_missing = true;
            let dropped = if matches!(&resolved_wiki, Some(r) if r.smart) {
                // Markerless smart wiki: a removed page's sections are
                // hard-dropped — the page is their only source, so there
                // is nothing to tombstone.
                usize::try_from(sections::drop_page_sections(pool, &source_path).await?)
                    .unwrap_or(usize::MAX)
            } else {
                drop_active_rows_for_source(pool, &source_path, spare_pending).await?
            };
            report.orphaned = dropped;
            if dropped > 0 {
                tracing::info!(
                    source_path = %source_path,
                    orphaned = dropped,
                    "reindex_file: FILE_REMOVED (rows removed)"
                );
            }
            return Ok(report);
        },
        Err(e) => return Err(e.into()),
    };

    let Some(resolved) = resolved_wiki else {
        // Outside any wiki — we still want to honour a file delete on
        // the source_path (handled above), but no fresh inserts make
        // sense. Just return a clean report.
        return Ok(report);
    };

    if resolved.smart {
        // Markerless smart wiki: index the page content by section into
        // `wiki_sections`. No ACL is stamped here — read access belongs to
        // the wiki, and recall resolves it once per wiki from the
        // `smart_wikis` registry.
        section_index_page(
            pool,
            embedder.as_ref(),
            &source_path,
            &resolved.wiki_id,
            &raw,
            &mut report,
        )
        .await?;
        return Ok(report);
    }

    // Standard wiki: DB-authoritative, offset-and-existence repair only —
    // per-fragment `{{f=…}}` markers stay the pillar here.
    let parsed = parser::parse(&raw);
    for w in &parsed.warnings {
        report
            .warnings
            .push(format!("{:?}@{}: {}", w.kind, w.offset, w.detail));
    }

    let on_disk = collect_disk_markers(&parsed.events);
    let active_rows = fact_index::find_active_by_source_path(pool, &source_path).await?;
    let active_by_id: std::collections::HashMap<&str, &FactIndexRow> = active_rows
        .iter()
        .map(|r| (r.fact_id.as_str(), r))
        .collect();
    let on_disk_ids: HashSet<&str> = on_disk.iter().map(|m| m.fact_id.as_str()).collect();

    for marker in &on_disk {
        let existing = active_by_id.get(marker.fact_id.as_str()).copied();
        apply_standard_marker(
            pool,
            &source_path,
            &resolved.wiki_id,
            marker,
            existing,
            &mut report,
        )
        .await?;
    }

    apply_orphan_sweep(
        pool,
        &source_path,
        &resolved.wiki_id,
        &active_rows,
        &on_disk_ids,
        spare_pending,
        &mut report,
    )
    .await?;

    Ok(report)
}

// ---------- strip_fact_region (retire-time page cleanup) ----------

/// Remove a retired fact's `{{f=id}}…{{/}}` region from its on-disk page,
/// then re-sync the surviving markers' offsets via [`reindex_file`].
///
/// This is the disk half of retirement: the DB tombstone
/// (`superseded_at` / `deleted_at`) keeps the fact out of recall, and this
/// keeps the stale bytes out of the raw page that recall-by-navigation
/// reads — and out of the export, which rewrites whatever regions remain on
/// disk to their full-marker form on purpose (retired residue included,
/// via the full [`fact_index::page_acl_map`]). Call it **after** the row is
/// retired: the function refuses to touch an active fact (excising a live
/// region would let the next reindex orphan-sweep tombstone the fact), so
/// a caller racing a concurrent revert can never corrupt live prose.
///
/// Every act-time retire path funnels here: `capture::wiki_supersede`,
/// `capture::wiki_forget` (the consumer/dashboard/comment forget sites),
/// the light-dream supersede hint, and the REM revisor's direct dedup
/// merge. Retire paths with no engine context at hand (the proposal apply
/// chassis — `fact_forget` votes resolving inside it, a pending
/// `dedup_merge` apply) leave the bytes for the light-dream hygiene sweep
/// ([`sweep_retired_regions`]) instead. On a successful excision the
/// retired row's offsets are settled to NULL so the sweep's candidate
/// query converges.
///
/// **Best-effort and self-guarding.** Returns `Ok(false)` (no error) when
/// there is nothing safe to strip: the row is gone or still active, has no
/// tracked region (NULL offsets — a smart wiki or a pending render), the
/// page cannot be read, or the stored offsets no longer bracket this
/// fact's marker (stale offsets — left for the page-level sweep, which
/// re-parses instead of trusting offsets). Returns `Ok(true)` when a
/// region was excised and the page rewritten.
///
/// Known write-path caveat: page writes are not serialized per page, so
/// two concurrent strips (or a strip racing a compile) can lose one
/// excision — degraded, not corrupting; the wider fix is the concurrency
/// hardening tracked in the roadmap (group 4e).
///
/// # Errors
///
/// Propagates a genuine write / reindex failure ([`ReindexError`]); the
/// row lookup and page read soft-fail to `Ok(false)`.
pub async fn strip_fact_region(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    fact_id: &FactId,
) -> Result<bool> {
    let Some(row) = fact_index::find_by_id(pool, fact_id).await? else {
        return Ok(false);
    };
    // Retired rows only: excising an ACTIVE fact's region would erase live
    // prose and hand the fact to the marker-removed orphan sweep.
    if row.superseded_at.is_none() && row.deleted_at.is_none() {
        return Ok(false);
    }
    let (Some(start), Some(end)) = (row.region_start, row.region_end) else {
        return Ok(false);
    };
    // Negative / oversized offsets are stale — nothing safe to strip.
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return Ok(false);
    };
    let abs = tree.workdir().join(&row.source_path);
    let Ok(raw) = std::fs::read_to_string(&abs) else {
        return Ok(false);
    };
    // Stale-offset guard: the stored span must actually bracket THIS fact's
    // marker before we cut, or we would corrupt the page.
    if start >= end || end > raw.len() || !raw.is_char_boundary(start) || !raw.is_char_boundary(end)
    {
        return Ok(false);
    }
    let span = &raw[start..end];
    let open = format!("{{{{f={}}}}}", fact_id.as_str());
    if !span.starts_with(&open) || !span.ends_with("{{/}}") {
        return Ok(false);
    }
    let mut out = String::with_capacity(raw.len() - (end - start));
    out.push_str(&raw[..start]);
    out.push_str(&raw[end..]);
    crate::wiki::atomic_write(&abs, out.as_bytes())?;
    // Re-sync the surviving markers' byte offsets on this page.
    reindex_file(pool, tree, embedder, &abs).await?;
    // Settle the retired row: its bytes are gone, so NULL the offsets
    // (also drops it out of the hygiene-sweep candidate set).
    fact_index::clear_region_offsets_retired(pool, fact_id).await?;
    Ok(true)
}

// ---------- Retired-region hygiene sweep (light-dream disk half) ----------

/// Resource cap: pages one [`sweep_retired_regions`] pass will open.
///
/// Excess candidates simply wait for the next light cycle — the sweep is
/// a convergent backstop, not a deadline. A bound on per-cycle IO, not a
/// semantic gate.
pub const RETIRED_SWEEP_MAX_PAGES: usize = 64;

/// Outcome of one [`sweep_retired_regions`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetiredSweepReport {
    /// Candidate pages opened this pass (after the plan/cap filters).
    pub pages_examined: usize,
    /// Retired regions excised from disk.
    pub regions_stripped: usize,
    /// Retired rows whose stale offsets were settled to NULL (their marker
    /// was excised now, or already absent from the page).
    pub rows_settled: u64,
    /// Candidate pages skipped because the current compilation plan owns
    /// them (they self-clean at their next compile).
    pub pages_skipped_plan: usize,
    /// Per-page soft errors; the sweep continues.
    pub warnings: Vec<String>,
}

impl RetiredSweepReport {
    /// Whether the pass did (or found) anything worth logging.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.regions_stripped + self.pages_examined > 0 || self.rows_settled > 0
    }
}

/// Excise every retired fact's region from one page.
///
/// Re-parses the page instead of trusting stored offsets (robust against
/// drift), then settles the offsets of the page's retired rows whose
/// marker is no longer on disk.
///
/// The page-level sibling of [`strip_fact_region`], used by
/// [`sweep_retired_regions`] for residue whose retire path could not strip
/// act-time (the proposal apply chassis) or whose stored offsets went
/// stale. Markers whose fact is **active**, and markers with no row at
/// all, are left untouched (the reindex sweep's business, not
/// retirement's). A missing page settles its retired rows and excises
/// nothing.
///
/// Returns `(regions_stripped, rows_settled)`.
///
/// # Errors
///
/// Propagates filesystem read failures other than not-found, write /
/// reindex failures, and DB failures.
pub async fn strip_retired_regions_on_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    source_path: &str,
) -> Result<(usize, u64)> {
    let abs = tree.workdir().join(source_path);
    // Reserved plumbing pages never carry fact regions; nothing to excise.
    if is_reserved_page(&abs) {
        let settled = fact_index::clear_region_offsets_retired_on_page(pool, source_path).await?;
        return Ok((0, settled));
    }
    let raw = match std::fs::read_to_string(&abs) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Page gone from disk: no bytes can remain — settle every
            // retired row still pointing here so the sweep converges.
            let settled =
                fact_index::clear_region_offsets_retired_on_page(pool, source_path).await?;
            return Ok((0, settled));
        },
        Err(e) => return Err(e.into()),
    };

    // Fresh parse: which on-disk regions belong to retired facts?
    let parsed = parser::parse(&raw);
    let markers = collect_disk_markers(&parsed.events);
    let mut cut: Vec<(usize, usize)> = Vec::new();
    for m in &markers {
        let Some(row) = fact_index::find_by_id(pool, &m.fact_id).await? else {
            continue; // no row: reindex's stale-render residue, not ours
        };
        if row.superseded_at.is_some() || row.deleted_at.is_some() {
            cut.push((m.region_start, m.region_end));
        }
    }

    let stripped = cut.len();
    if stripped > 0 {
        cut.sort_unstable();
        let mut out = String::with_capacity(raw.len());
        let mut pos = 0usize;
        for (start, end) in cut {
            // Parser spans are ordered and non-overlapping; the guard is
            // pure defence against a malformed event stream.
            if start < pos || end > raw.len() || start >= end {
                continue;
            }
            out.push_str(&raw[pos..start]);
            pos = end;
        }
        out.push_str(&raw[pos..]);
        crate::wiki::atomic_write(&abs, out.as_bytes())?;
        // Re-sync the surviving markers' byte offsets on this page.
        reindex_file(pool, tree, embedder, &abs).await?;
    }

    // Settle: any retired row still holding offsets here whose marker is
    // (now) absent from the page drops them, so it stops nominating this
    // page as a sweep candidate.
    let after = if stripped > 0 {
        std::fs::read_to_string(&abs).unwrap_or_default()
    } else {
        raw
    };
    let mut settled = 0u64;
    for id in fact_index::retired_region_fact_ids_on_page(pool, source_path).await? {
        if after.contains(&format!("{{{{f={id}}}}}")) {
            continue; // still on disk (excision raced/failed) — retry next pass
        }
        let Ok(fid) = FactId::parse(&id) else {
            continue; // malformed legacy id: leave it; never abort the pass
        };
        settled += fact_index::clear_region_offsets_retired(pool, &fid).await?;
    }
    Ok((stripped, settled))
}

/// The light-dream **retirement hygiene sweep**.
///
/// Excises retired-fact regions left on pages **outside the current
/// compilation plan** — `rules.md` (the compiler never rewrites it) and
/// husk pages — where residue is otherwise permanent. Pages the plan owns
/// are skipped: the compiler rewrites them from the active fact set at
/// their next compile, so their residue self-cleans.
///
/// This is the convergent backstop behind the act-time strips (see
/// [`strip_fact_region`]): the retire paths that run inside the proposal
/// apply chassis (a `fact_forget` vote resolving, a pending `dedup_merge`
/// applied manually or by the overdue auto-apply) have no
/// tree/embedder at hand and deliberately do not strip — their residue is
/// picked up here on the next light cycle. Residue is never a leak
/// meanwhile: the reader paths load the active ACL map, so a retired
/// region on disk redacts fail-closed.
///
/// Candidates come from retired rows still holding region offsets
/// ([`fact_index::retired_region_pages`]); each processed page settles
/// those offsets, so a page is revisited only while it still has work.
/// At most `max_pages` pages are opened per pass
/// ([`RETIRED_SWEEP_MAX_PAGES`] at the light-cycle call site). If the
/// compilation plan cannot be loaded the sweep skips the whole pass
/// (without the plan it cannot tell residue that self-cleans from residue
/// that never will).
///
/// # Errors
///
/// Propagates only infrastructure failures (the candidate query, the tree
/// walk); per-page failures are collected into `warnings`.
pub async fn sweep_retired_regions(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    max_pages: usize,
) -> Result<RetiredSweepReport> {
    let mut report = RetiredSweepReport::default();
    let candidates = fact_index::retired_region_pages(pool).await?;
    if candidates.is_empty() {
        return Ok(report);
    }
    let plan_pages = match plan_page_source_paths(tree) {
        Ok(set) => set,
        Err(e) => {
            report
                .warnings
                .push(format!("compilation plan unreadable — sweep skipped: {e}"));
            return Ok(report);
        },
    };
    for source_path in candidates {
        if plan_pages.contains(&source_path) {
            report.pages_skipped_plan += 1;
            continue;
        }
        if report.pages_examined >= max_pages {
            break; // bounded: the rest waits for the next light cycle
        }
        report.pages_examined += 1;
        match strip_retired_regions_on_page(pool, tree, embedder.clone(), &source_path).await {
            Ok((stripped, settled)) => {
                report.regions_stripped += stripped;
                report.rows_settled += settled;
                if stripped > 0 {
                    tracing::info!(
                        source_path = %source_path,
                        stripped,
                        "retired-region sweep: residue excised from non-plan page"
                    );
                }
            },
            Err(e) => {
                report.warnings.push(format!("{source_path}: {e}"));
                tracing::warn!(
                    source_path = %source_path,
                    error = %e,
                    "retired-region sweep: page failed (will retry next cycle)"
                );
            },
        }
    }
    Ok(report)
}

/// The workdir-relative `source_path` of every page in the persisted
/// compilation plan (`wikis/_plan/compilation-plan.json`). No plan on disk
/// yet (a fresh deployment) is an empty set — every page is non-plan.
fn plan_page_source_paths(tree: &WikiTree) -> anyhow::Result<HashSet<String>> {
    let Some(plan) = crate::planner::load_previous_plan(tree)? else {
        return Ok(HashSet::new());
    };
    let rel_by_wiki: std::collections::HashMap<String, String> = tree
        .walk()?
        .into_iter()
        .map(|d| {
            (
                d.meta.wiki_id.as_str().to_owned(),
                d.rel_dir.to_string_lossy().replace('\\', "/"),
            )
        })
        .collect();
    Ok(plan
        .pages
        .values()
        .filter_map(|p| {
            rel_by_wiki
                .get(&p.wiki_id)
                .map(|rel| format!("{rel}/{}", p.page_path))
        })
        .collect())
}

// ---------- smart_wikis registry projection ----------

/// Outcome of one [`project_smart_wiki_registry`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartRegistryReport {
    /// Smart wikis projected (inserted or refreshed).
    pub projected: usize,
    /// Registry rows dropped because their wiki is gone or no longer smart.
    pub removed: usize,
}

/// Re-project every smart wiki's `_meta.md` into the `smart_wikis`
/// registry, and drop registry rows whose wiki is gone or stopped being
/// smart.
///
/// `_meta.md` on disk stays the single source of truth: this table is a
/// **queryable cache** of it, which is why the projection re-runs on the
/// safety-net tick rather than being written once. What it buys is that
/// recall can ask the DB "which wikis are smart, and who may read them?"
/// instead of resolving every hit's wiki through a tree walk — that
/// impossibility is why the smart filter used to be applied *after*
/// top-K, silently shrinking the caller's result set.
///
/// Idempotent: an unchanged wiki re-writes the same values.
///
/// # Errors
///
/// Propagates the tree walk and DB failures.
pub async fn project_smart_wiki_registry(
    pool: &SqlitePool,
    tree: &WikiTree,
) -> anyhow::Result<SmartRegistryReport> {
    let mut report = SmartRegistryReport::default();
    let mut live: HashSet<String> = HashSet::new();

    for d in tree.walk()? {
        if !d.meta.smart {
            continue;
        }
        let wiki_id = d.meta.wiki_id.as_str().to_owned();
        let owner_id = match tree.resolve_scope_principal(&d.meta) {
            Ok(p) => p,
            Err(e) => {
                // An unresolvable scope means we cannot say who may read
                // the wiki. Leaving the row out is fail-closed: recall
                // simply will not offer its sections.
                tracing::warn!(
                    wiki_id = %wiki_id,
                    error = %e,
                    "smart registry: scope unresolved — wiki left out of the registry"
                );
                continue;
            },
        };
        let project_id = d
            .meta
            .extra
            .get(serde_yaml::Value::from("project_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        sections::upsert_smart_wiki(
            pool,
            &sections::SmartWikiRow {
                wiki_id: wiki_id.clone(),
                slug: d.meta.slug.as_str().to_owned(),
                owner_id,
                shared_with: d.meta.shared_with.clone(),
                project_id,
                wiki_type: d.meta.wiki_type.clone(),
            },
        )
        .await?;
        live.insert(wiki_id);
        report.projected += 1;
    }

    for row in sections::list_smart_wikis(pool).await? {
        if !live.contains(&row.wiki_id) {
            sections::remove_smart_wiki(pool, &row.wiki_id).await?;
            report.removed += 1;
            tracing::info!(
                wiki_id = %row.wiki_id,
                "smart registry: row dropped — wiki gone or no longer smart"
            );
        }
    }
    Ok(report)
}

// ---------- Boot-time smart-section backfill ----------

/// Outcome of one [`backfill_smart_sections`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartBackfillReport {
    /// Pages whose legacy rows were moved into `wiki_sections`.
    pub pages_moved: usize,
    /// Section rows written.
    pub sections_written: usize,
    /// Legacy `fact_index` rows removed.
    pub legacy_rows_dropped: usize,
}

/// One-time boot pass: move legacy smart-wiki content rows out of
/// `fact_index` and into `wiki_sections`.
///
/// Smart-wiki sections used to live in `fact_index` alongside standard
/// facts (see [`crate::sections`] for why they no longer do). This moves
/// them, **copying the stored embeddings verbatim** — no re-embedding, so
/// the migration costs one pass over the rows and nothing else.
///
/// Legacy rows carry no ordinal (they were keyed by a minted id), so the
/// position is reconstructed by sorting each page's rows by `fact_id` —
/// `UUIDv7` is time-ordered, so that is their original insertion order.
/// Any residual drift is free to fix: the next reindex re-derives the
/// true order from disk and reuses each section's stored vector by text,
/// so no embedding is recomputed there either.
///
/// Idempotent. A page already present in `wiki_sections` is not
/// re-copied — its legacy rows are simply dropped — so a re-run after a
/// partial pass converges, and a run on an already-migrated store is a
/// no-op.
///
/// # Errors
///
/// Propagates the tree walk and DB failures.
pub async fn backfill_smart_sections(
    pool: &SqlitePool,
    tree: &WikiTree,
) -> anyhow::Result<SmartBackfillReport> {
    let mut report = SmartBackfillReport::default();

    for d in tree.walk()? {
        if !d.meta.smart {
            continue;
        }
        let wiki_id = d.meta.wiki_id.as_str().to_owned();
        let legacy = fact_index::find_active_in_wiki(pool, &wiki_id).await?;
        if legacy.is_empty() {
            continue;
        }

        // Group the page's rows, then order them the way they were
        // inserted (UUIDv7 is time-ordered).
        let mut by_page: BTreeMap<String, Vec<&FactIndexRow>> = BTreeMap::new();
        for row in &legacy {
            by_page
                .entry(row.source_path.clone())
                .or_default()
                .push(row);
        }

        for (source_path, mut rows) in by_page {
            rows.sort_by(|a, b| a.fact_id.as_str().cmp(b.fact_id.as_str()));

            let already = sections::find_page_sections(pool, &source_path).await?;
            if already.is_empty() {
                let new_sections: Vec<sections::NewSection> = rows
                    .iter()
                    .enumerate()
                    .map(|(ord, row)| sections::NewSection {
                        wiki_id: wiki_id.clone(),
                        source_path: source_path.clone(),
                        section_ord: i64::try_from(ord).unwrap_or(i64::MAX),
                        // Unknown for a legacy row — the heading chain is
                        // baked into `text`. The next reindex fills it in.
                        heading_path: None,
                        text: row.text.clone(),
                        embedding: row.embedding.clone(),
                    })
                    .collect();
                let (written, _) =
                    sections::replace_page_sections(pool, &source_path, &new_sections).await?;
                report.sections_written += usize::try_from(written).unwrap_or(usize::MAX);
                report.pages_moved += 1;
            }

            let dropped = fact_index::drop_by_source_path(pool, &source_path).await?;
            report.legacy_rows_dropped += usize::try_from(dropped).unwrap_or(usize::MAX);
        }
    }

    if report.pages_moved > 0 || report.legacy_rows_dropped > 0 {
        tracing::info!(
            pages_moved = report.pages_moved,
            sections_written = report.sections_written,
            legacy_rows_dropped = report.legacy_rows_dropped,
            "smart backfill: legacy smart-wiki rows moved out of fact_index"
        );
    }
    Ok(report)
}

// ---------- Boot-time wiki_id reconcile (safety net) ----------

/// Outcome of one [`reconcile_wiki_ids`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WikiIdReconcileReport {
    /// Active rows examined.
    pub scanned: usize,
    /// Rows whose `wiki_id` was repointed to their `source_path`'s wiki.
    pub fixed: usize,
    /// Rows whose `source_path` falls under no discovered wiki (left
    /// untouched, logged at WARN).
    pub unknown: usize,
}

/// Boot-time safety net: re-derive every **active** fact row's wiki from
/// its `source_path` and fix `wiki_id` where the two diverged.
///
/// The wiki of a path is the discovered wiki whose directory is the
/// **longest prefix** of the row's `source_path` — sub-wikis nest
/// (`wikis/famiglia/bruno-battaglia/…` belongs to
/// `famiglia-bruno-battaglia`, not `famiglia`), which is why this is a
/// tree-aware Rust pass and not a SQL migration: only the walked `_meta.md`
/// set knows the sub-wiki directories. Prefixes match on a directory
/// boundary (trailing `/`), so `wikis/alice-bis/…` can never fall under
/// `wikis/alice`.
///
/// Idempotent: a consistent row is untouched, each fix is a targeted
/// UPDATE guarded on the divergent value it corrects
/// ([`fact_index::set_wiki_id`]) and logged at INFO; a row under no known
/// wiki is left as-is and logged at WARN. Runs once from `serve` startup,
/// after the tree opens.
///
/// # Errors
///
/// Propagates the tree walk and DB failures.
pub async fn reconcile_wiki_ids(
    pool: &SqlitePool,
    tree: &WikiTree,
) -> anyhow::Result<WikiIdReconcileReport> {
    // (dir prefix with trailing slash, wiki_id) for every discovered wiki.
    let prefixes: Vec<(String, String)> = tree
        .walk()?
        .into_iter()
        .map(|d| {
            let mut p = d.rel_dir.to_string_lossy().replace('\\', "/");
            p.push('/');
            (p, d.meta.wiki_id.as_str().to_owned())
        })
        .collect();

    let rows = fact_index::all_active_locations(pool).await?;
    let mut report = WikiIdReconcileReport {
        scanned: rows.len(),
        ..Default::default()
    };
    for row in rows {
        let derived = prefixes
            .iter()
            .filter(|(prefix, _)| row.source_path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, wiki_id)| wiki_id);
        match derived {
            None => {
                report.unknown += 1;
                tracing::warn!(
                    fact_id = %row.fact_id,
                    wiki_id = %row.wiki_id,
                    source_path = %row.source_path,
                    "wiki-id reconcile: source_path under no known wiki — row left as-is"
                );
            },
            Some(wiki_id) if *wiki_id == row.wiki_id => {},
            Some(wiki_id) => {
                let touched =
                    fact_index::set_wiki_id(pool, &row.fact_id, &row.wiki_id, wiki_id).await?;
                if touched > 0 {
                    report.fixed += 1;
                    tracing::info!(
                        fact_id = %row.fact_id,
                        from = %row.wiki_id,
                        to = %wiki_id,
                        source_path = %row.source_path,
                        "wiki-id reconcile: row repointed to its source_path's wiki"
                    );
                }
            },
        }
    }
    if report.fixed > 0 || report.unknown > 0 {
        tracing::info!(
            scanned = report.scanned,
            fixed = report.fixed,
            unknown = report.unknown,
            "wiki-id reconcile: pass complete"
        );
    }
    Ok(report)
}

// ---------- reindex_full ----------

/// Walk every wiki under `tree`, scan every `*.md` page, and reconcile
/// `fact_index` row-by-row. The slow safety-net path.
///
/// # Errors
///
/// Per-file `ReindexError`s are converted into a report warning instead
/// of aborting the walk; only the initial `tree.walk()` failure surfaces
/// as a hard error.
pub async fn reindex_full(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
) -> Result<ReindexFullReport> {
    let mut report = ReindexFullReport::default();
    // Refresh the `smart_wikis` registry first: it is a projection of the
    // `_meta.md` files this sweep is about to walk, so a hand edit to a
    // wiki's `smart:` flag or `shared_with:` roster lands here — including
    // a revoke, which must close the recall window on the next tick even
    // if nobody hit the dashboard sharing route.
    match project_smart_wiki_registry(pool, tree).await {
        Ok(r) if r.projected > 0 || r.removed > 0 => tracing::debug!(
            projected = r.projected,
            removed = r.removed,
            "reindex_full: smart registry refreshed"
        ),
        Ok(_) => {},
        Err(e) => tracing::warn!(error = %e, "reindex_full: smart registry projection failed"),
    }
    // Standard wikis are compiler OUTPUT — their fact_index is owned by
    // the buffer→promote→compile chain. `reindex_file` is standard-wiki-safe
    // per event (offset-and-existence repair only), but the periodic tick
    // must still SKIP standard pages: unlike the watcher it has no
    // own-write suppression, so it can observe a mid-compile window (a
    // fact moved off page A whose row is repointed only when page B
    // compiles) and would tombstone a live row. We rebuild the captures
    // buffer from the journal for every wiki, then section-index only
    // smart wikis (smart-consumer-owned, content-indexed plain markdown);
    // "standard" = "not smart" now that the `wiki_type` registry is
    // retired.
    let discovered = tree.walk()?;
    for d in &discovered {
        // Captures-journal recovery: rebuild the captures buffer DB index
        // from the durable per-wiki `_captures.md` journal. Best-effort — a
        // malformed journal must not abort the marker reindex.
        match crate::capture_buffer::reindex_capture_journal(pool, &d.meta.wiki_id, &d.abs_dir)
            .await
        {
            Ok(n) if n > 0 => tracing::info!(
                wiki_id = %d.meta.wiki_id,
                rebuilt = n,
                "reindex_full: captures buffer rebuilt from journal"
            ),
            Ok(_) => {},
            Err(e) => tracing::warn!(
                error = %e,
                wiki_id = %d.meta.wiki_id,
                "reindex_full: captures-journal rebuild error"
            ),
        }
        // Standard wikis: skip the content sweep (compiler output — see above).
        if !d.meta.smart {
            continue;
        }
        let pages = enumerate_pages(&d.abs_dir)?;
        for page in pages {
            report.files_scanned += 1;
            match reindex_file(pool, tree, embedder.clone(), &page).await {
                Ok(per) => {
                    report.total_inserted += per.inserted;
                    report.total_updated += per.updated;
                    report.total_orphaned += per.orphaned;
                    if per.changed() {
                        report.per_file.push(per);
                    }
                },
                Err(e) => {
                    let source_path = relative_source_path(tree, &page).unwrap_or_else(|_| {
                        crate::wiki::workdir_relative_source_path(tree.workdir(), &page)
                    });
                    report.per_file.push(ReindexFileReport {
                        source_path,
                        wiki_id: Some(d.meta.wiki_id.as_str().to_owned()),
                        warnings: vec![format!("reindex error: {e}")],
                        ..Default::default()
                    });
                    tracing::warn!(error = %e, page = %page.display(), "reindex_full: per-file error");
                },
            }
        }

        // Markerless deletion safety net: hard-drop the sections of any
        // page that no longer exists on disk (a `Removed` watcher event
        // this periodic tick is recovering). Smart wikis carry no
        // tombstone — a removed page's sections simply disappear.
        match sections::indexed_pages(pool).await {
            Ok(pages) => {
                let mut gone: HashSet<String> = HashSet::new();
                for (wiki_id, source_path) in pages {
                    if wiki_id != d.meta.wiki_id.as_str() || gone.contains(&source_path) {
                        continue;
                    }
                    if !tree.workdir().join(&source_path).exists() {
                        gone.insert(source_path);
                    }
                }
                for sp in &gone {
                    match sections::drop_page_sections(pool, sp).await {
                        Ok(n) => report.total_orphaned += usize::try_from(n).unwrap_or(usize::MAX),
                        Err(e) => tracing::warn!(
                            error = %e,
                            source_path = %sp,
                            "reindex_full: smart deleted-page drop failed"
                        ),
                    }
                }
            },
            Err(e) => tracing::warn!(
                error = %e,
                wiki_id = %d.meta.wiki_id,
                "reindex_full: smart deleted-page sweep query failed"
            ),
        }
    }
    if report.total_inserted + report.total_updated + report.total_orphaned > 0 {
        tracing::info!(
            files = report.files_scanned,
            inserted = report.total_inserted,
            updated = report.total_updated,
            orphaned = report.total_orphaned,
            "reindex_full: done"
        );
    }
    Ok(report)
}

// ---------- watcher loop ----------

/// Consume a [`watcher::WikiWatcher`] event stream and route every event
/// through [`reindex_file`].
///
/// Renames are handled as a touch on the destination + a possible
/// delete on the source (matches the parser-agnostic semantics: the
/// watcher's marker filter already suppresses spurious events from our
/// own atomic-rename writes).
///
/// Spawn it on a `tokio` runtime; the future returns when the channel
/// closes (the parent `WikiWatcher` was dropped).
pub async fn run_watcher_loop(
    pool: SqlitePool,
    tree: Arc<WikiTree>,
    embedder: Arc<dyn Embedder>,
    mut rx: mpsc::UnboundedReceiver<WatchedChange>,
) {
    while let Some(change) = rx.recv().await {
        match change {
            WatchedChange::Touched(p) | WatchedChange::Removed(p) => {
                if !is_markdown_page(&p) {
                    continue;
                }
                if let Err(e) = reindex_file(&pool, &tree, embedder.clone(), &p).await {
                    tracing::warn!(error = %e, path = %p.display(), "watcher: reindex_file failed");
                }
            },
            WatchedChange::Renamed { from, to } => {
                for p in [&from, &to] {
                    if !is_markdown_page(p) {
                        continue;
                    }
                    if let Err(e) = reindex_file(&pool, &tree, embedder.clone(), p).await {
                        tracing::warn!(error = %e, path = %p.display(), "watcher: reindex_file failed");
                    }
                }
            },
        }
    }
}

/// Spawn [`run_watcher_loop`] on the current `tokio` runtime and return
/// the [`JoinHandle`] so the caller can keep it alive for the program's
/// lifetime (the loop exits when the channel closes).
#[must_use]
pub fn spawn_watcher_loop(
    pool: SqlitePool,
    tree: Arc<WikiTree>,
    embedder: Arc<dyn Embedder>,
    rx: mpsc::UnboundedReceiver<WatchedChange>,
) -> JoinHandle<()> {
    tokio::spawn(run_watcher_loop(pool, tree, embedder, rx))
}

// ---------- safety net loop ----------

/// Re-run [`reindex_full`] on a fixed interval as a safety net against
/// missed `notify` events (NFS, suspended laptops, brief crashes).
///
/// The loop runs forever; cancel it by dropping the returned
/// [`JoinHandle`] / aborting the task. Errors from individual sweeps
/// are logged at `warn` level and the loop continues.
pub async fn run_safety_net_loop(
    pool: SqlitePool,
    tree: Arc<WikiTree>,
    embedder: Arc<dyn Embedder>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    // Discard the immediate first tick: the watcher has just been
    // armed, do not slam the embedder before any third-party edit had
    // a chance to fire.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match reindex_full(&pool, &tree, embedder.clone()).await {
            Ok(report) if report.files_scanned > 0 => {
                tracing::debug!(
                    files = report.files_scanned,
                    inserted = report.total_inserted,
                    updated = report.total_updated,
                    orphaned = report.total_orphaned,
                    "reindex safety net tick"
                );
            },
            Ok(_) => {},
            Err(e) => tracing::warn!(error = %e, "reindex safety net error"),
        }
    }
}

/// Spawn [`run_safety_net_loop`] on the current `tokio` runtime.
#[must_use]
pub fn spawn_safety_net_loop(
    pool: SqlitePool,
    tree: Arc<WikiTree>,
    embedder: Arc<dyn Embedder>,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(run_safety_net_loop(pool, tree, embedder, interval))
}

// ---------- internals ----------

/// Workdir-relative source path encoded with forward slashes (matches
/// the format `capture::wiki_capture` writes). Returns
/// [`ReindexError::PathOutsideTree`] when `abs_path` is not under the
/// workdir, so the watcher loop never silently re-indexes a stranger.
///
/// Both `tree.workdir()` and `abs_path` are canonicalised on the fly
/// before the prefix check so the comparison survives macOS's
/// `/private/var/...` `FSEvents` quirk and the Windows long-name vs
/// short-name (`RUNNER~1` ↔ `runneradmin`) mismatch. We deliberately
/// resolve at the helper boundary instead of pre-canonicalising the
/// tree's stored workdir so the operator-facing API on [`WikiTree`]
/// stays a no-surprises mirror of the input.
fn relative_source_path(tree: &WikiTree, abs_path: &Path) -> Result<String> {
    let workdir_canon = canonicalize_or_raw(tree.workdir());
    let abs_canon = canonicalize_or_raw(abs_path);
    if abs_canon.strip_prefix(&workdir_canon).is_err() {
        return Err(ReindexError::PathOutsideTree(abs_path.to_path_buf()));
    }
    Ok(crate::wiki::workdir_relative_source_path(
        &workdir_canon,
        &abs_canon,
    ))
}

/// The owning wiki of a watched path, resolved for the reindex sweep.
///
/// Carries no ACL: a smart wiki's read access lives once in the
/// `smart_wikis` registry (projected by
/// [`project_smart_wiki_registry`]), not stamped onto each section, and a
/// standard wiki's lives per fragment in `fact_index`.
struct ResolvedWiki {
    wiki_id: String,
    /// Picks the sweep shape: smart = content-indexed by section;
    /// standard = DB-authoritative, offset-and-existence repair.
    smart: bool,
}

/// Resolve the owning wiki of `abs_path` (see [`ResolvedWiki`]).
fn resolve_wiki_for_path(tree: &WikiTree, abs_path: &Path) -> Result<Option<ResolvedWiki>> {
    let abs_canon = canonicalize_or_raw(abs_path);
    let discovered = tree.walk()?;
    let Some(d) = pick_wiki_for_path(&discovered, &abs_canon) else {
        return Ok(None);
    };
    Ok(Some(ResolvedWiki {
        wiki_id: d.meta.wiki_id.as_str().to_owned(),
        smart: d.meta.smart,
    }))
}

/// Canonicalise `p` if possible, otherwise fall back to the raw input.
///
/// `std::fs::canonicalize` fails on a non-existent path — important for
/// the delete branch of `reindex_file` where the watcher hands us the
/// path of a freshly-removed file. We recover by canonicalising the
/// parent directory (still on disk) and re-joining the filename, so the
/// prefix check survives the deletion. As a last resort we hand back
/// the raw path so the helper never panics; the `strip_prefix` check
/// will then catch the mismatch as a normal `PathOutsideTree`.
fn canonicalize_or_raw(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
        && let Ok(parent_canon) = parent.canonicalize()
    {
        return parent_canon.join(name);
    }
    p.to_path_buf()
}

fn pick_wiki_for_path<'a>(
    discovered: &'a [DiscoveredWiki],
    abs_path: &Path,
) -> Option<&'a DiscoveredWiki> {
    let abs_canon = canonicalize_or_raw(abs_path);
    let mut best: Option<&DiscoveredWiki> = None;
    let mut best_depth = 0usize;
    for d in discovered {
        // Canonicalise `d.abs_dir` too: `tree.walk()` returns the raw
        // joined path (rooted at the operator-supplied `wikis_dir`),
        // but `abs_canon` is canonical — without this step the
        // `starts_with` check fails on macOS (`/var/...` vs
        // `/private/var/...`) and on Windows (short-name `RUNNER~1`
        // vs canonical `runneradmin`).
        let d_canon = canonicalize_or_raw(&d.abs_dir);
        if abs_canon.starts_with(&d_canon) {
            let depth = d_canon.components().count();
            if best.is_none() || depth > best_depth {
                best = Some(d);
                best_depth = depth;
            }
        }
    }
    best
}

fn is_markdown_page(p: &Path) -> bool {
    p.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// The captures journal (`_captures.md`) is a `.md` file but NOT a
/// publishable page: it carries buffered-capture entries, not `{{f=…}}` fact
/// regions. The marker reindex must never index it into `fact_index` (its
/// own rebuild path is `crate::capture_buffer::reindex_capture_journal`).
fn is_capture_journal(p: &Path) -> bool {
    p.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|n| n == crate::wiki::CAPTURES_FILENAME)
}

/// `_meta.md` is the wiki's config frontmatter, never an indexable page —
/// the section-indexer must skip it so its YAML never becomes fact rows.
fn is_meta_file(p: &Path) -> bool {
    p.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|n| n == crate::wiki::META_FILENAME)
}

/// The smart consumer's feedback INBOX (`_briefing.md` + its rotated
/// `_briefing.archive.md`) — addressed TO the consumer, not knowledge it
/// authored, so the section-indexer must never turn it into recallable
/// facts. (`_briefing.md` is smart-wiki-only; skipping it on a standard
/// wiki is harmless — that path never indexes from disk anyway.)
fn is_briefing_file(p: &Path) -> bool {
    p.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|n| n == crate::briefing::BRIEFING_FILENAME || n == "_briefing.archive.md")
}

/// Underscore-prefixed pages reserved by the engine — never indexable
/// content. Centralised so a newly-reserved page can't be missed by one
/// reindex path (the watcher fast path **and** the periodic full sweep).
fn is_reserved_page(p: &Path) -> bool {
    is_capture_journal(p) || is_meta_file(p) || is_briefing_file(p)
}

fn enumerate_pages(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    enumerate_pages_inner(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn enumerate_pages_inner(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in read_dir {
        let entry = entry?;
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_dir() {
            // Skip mwe-mcp internal scratch dirs the snapshot/recovery
            // pipeline (`_snapshots/`) writes alongside the wiki body.
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("_snapshots") {
                continue;
            }
            enumerate_pages_inner(&path, out)?;
        } else if ft.is_file() && is_markdown_page(&path) && !is_reserved_page(&path) {
            out.push(path);
        }
    }
    Ok(())
}

async fn drop_active_rows_for_source(
    pool: &SqlitePool,
    source_path: &str,
    spare_pending: bool,
) -> Result<usize> {
    let rows = fact_index::find_active_by_source_path(pool, source_path).await?;
    let mut count = 0usize;
    for row in &rows {
        if spare_pending && row.region_start.is_none() {
            // Pending render on a standard page: the fact was committed
            // but never rendered into the deleted file, so the deletion
            // gesture cannot apply to it.
            continue;
        }
        let touched = fact_index::mark_forgotten(pool, &row.fact_id, REASON_FILE_REMOVED).await?;
        if touched > 0 {
            count += 1;
        }
    }
    Ok(count)
}

/// A `{{f=…}}` region read off a **standard** page — fact key + byte
/// span only. The standard sweep repairs offsets; the ACL and the
/// canonical text are DB-authoritative, so the inline attributes are not
/// collected. (Smart wikis carry no markers — see [`section_index_page`].)
#[derive(Debug, Clone)]
struct DiskMarker {
    fact_id: FactId,
    region_start: usize,
    region_end: usize,
}

fn collect_disk_markers(events: &[ParseEvent]) -> Vec<DiskMarker> {
    let mut out = Vec::new();
    for ev in events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fact_id) = attrs.fact_id.clone()
        {
            out.push(DiskMarker {
                fact_id,
                region_start: *start,
                region_end: *end,
            });
        }
    }
    out
}

/// Section text for one [`crate::document::Segment`]: the heading chain
/// (when present) prefixes the body so the heading words are part of the
/// indexed + embedded text.
fn segment_text(seg: &crate::document::Segment) -> String {
    match &seg.heading {
        Some(h) if !h.is_empty() => format!("{h}\n\n{}", seg.content),
        _ => seg.content.clone(),
    }
}

/// Whether a smart page's stored sections already match the desired ones,
/// **position by position** — the idempotent fast path that spares a
/// re-embed and a write on an unchanged page.
///
/// Position-sensitive on purpose: identity here *is* the position, so a
/// page whose sections were reordered is out of sync even though the same
/// texts are present.
fn smart_page_in_sync(existing: &[sections::SectionRow], desired: &[String]) -> bool {
    existing.len() == desired.len()
        && existing
            .iter()
            .enumerate()
            .all(|(ord, row)| row.section_ord == i64::try_from(ord).unwrap_or(i64::MAX))
        && existing
            .iter()
            .zip(desired.iter())
            .all(|(row, text)| &row.text == text)
}

/// The smart-wiki half of the reindex sweep — markerless content
/// indexing into `wiki_sections` (standard pages go through
/// [`apply_standard_marker`], which repairs `fact_index` instead).
///
/// Smart (project) wikis carry no per-fragment `{{f=…}}` markers: the
/// consumer writes plain markdown and recall indexes the content. The
/// page is chunked into heading-delimited sections (reusing the document
/// segmenter), each embedded, and the page's sections are replaced.
///
/// **No ACL is written here.** Read access to a section is the *wiki's*,
/// held once in the `smart_wikis` registry — which is why a sharing edit
/// no longer has to rewrite one row per section
/// ([`crate::sections`]).
///
/// Idempotent: an unchanged page mutates zero rows; an edit reuses the
/// stored vector of any section whose text is unchanged, re-embedding
/// only the rest.
async fn section_index_page(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    source_path: &str,
    wiki_id_str: &str,
    raw: &str,
    report: &mut ReindexFileReport,
) -> Result<()> {
    let policy = crate::document::DocumentPolicy::for_sections();
    let segments =
        crate::document::segment_document(raw, crate::document::DocFormat::Prose, None, &policy);
    // Dedup identical section texts: a page with two identical sections must
    // not produce two identical index rows, which would surface as the same
    // flat hit twice (same text, same score, distinct position).
    // Order-preserving — position is identity here.
    let mut desired: Vec<(String, Option<String>)> = Vec::new();
    for seg in &segments {
        let text = segment_text(seg);
        if !desired.iter().any(|(d, _)| d == &text) {
            let heading = seg.heading.clone().filter(|h| !h.is_empty());
            desired.push((text, heading));
        }
    }

    let existing = sections::find_page_sections(pool, source_path).await?;
    let desired_texts: Vec<String> = desired.iter().map(|(t, _)| t.clone()).collect();
    if smart_page_in_sync(&existing, &desired_texts) {
        return Ok(());
    }
    // Reuse the stored embedding for any section whose text is unchanged —
    // keyed by text, so a section that merely MOVED on the page is not
    // re-embedded either.
    let reuse: std::collections::HashMap<&str, &[f32]> = existing
        .iter()
        .map(|r| (r.text.as_str(), r.embedding.as_slice()))
        .collect();

    // Compute every section's embedding BEFORE touching the DB — the slow
    // embed I/O must stay out of the write transaction below.
    let mut new_sections: Vec<sections::NewSection> = Vec::with_capacity(desired.len());
    for (ord, (text, heading_path)) in desired.iter().enumerate() {
        let embedding = match reuse.get(text.as_str()) {
            Some(vec) => (*vec).to_vec(),
            None => embedder.embed(text).await?,
        };
        new_sections.push(sections::NewSection {
            wiki_id: wiki_id_str.to_owned(),
            source_path: source_path.to_owned(),
            section_ord: i64::try_from(ord).unwrap_or(i64::MAX),
            heading_path: heading_path.clone(),
            text: text.clone(),
            embedding,
        });
    }

    // Upsert by position and drop the tail in ONE transaction, so
    // concurrent reindexers of the same page (the push-enqueued index, the
    // filesystem watcher, the safety-net sweep) converge to a single clean
    // set instead of interleaving into duplicates.
    let (upserted, dropped) =
        sections::replace_page_sections(pool, source_path, &new_sections).await?;
    report.orphaned += usize::try_from(dropped).unwrap_or(usize::MAX);
    report.inserted += usize::try_from(upserted).unwrap_or(usize::MAX);

    if report.changed() {
        tracing::info!(
            wiki_id = %wiki_id_str,
            source_path = %source_path,
            sections = desired.len(),
            "reindex_file: smart wiki re-sectioned (content-indexed)"
        );
    }
    Ok(())
}

/// The standard-wiki half of the marker sweep: pure bookkeeping repair,
/// never reconstruction — the DB is the authoritative fact store and
/// standard pages are render output (compiler prose / capture appends).
///
/// - A marker with no row is stale render residue (or a hand-pasted
///   marker — facts cannot be authored by editing standard-wiki prose); it
///   is left alone for the next compile to rewrite.
/// - A marker with a row repairs the row's region offsets when they
///   drifted — anything that moved bytes above the region: a
///   server-side append on the same page (a rules-channel write shifts
///   every region below it), a compile rewrite, or a hand edit. The
///   row's `text` is the canonical claim and the prose span is a
///   different string by design, so the body is never compared, never
///   copied, and never re-embedded.
async fn apply_standard_marker(
    pool: &SqlitePool,
    source_path: &str,
    wiki_id_str: &str,
    marker: &DiskMarker,
    existing: Option<&FactIndexRow>,
    report: &mut ReindexFileReport,
) -> Result<()> {
    let Some(row) = existing else {
        tracing::debug!(
            wiki_id = %wiki_id_str,
            source_path = %source_path,
            fact_id = marker.fact_id.as_str(),
            "reindex_file: standard-wiki marker without a row — left to the next compile (DB is authoritative)"
        );
        return Ok(());
    };
    let start = i64::try_from(marker.region_start).unwrap_or(i64::MAX);
    let end = i64::try_from(marker.region_end).unwrap_or(i64::MAX);
    if row.region_start == Some(start) && row.region_end == Some(end) {
        return Ok(());
    }
    let touched =
        fact_index::move_region(pool, &marker.fact_id, source_path, Some(start), Some(end)).await?;
    if touched > 0 {
        report.updated += 1;
        // Neutral wording on purpose: offsets drift on ANY byte movement
        // above the region — most often the server's own writes (a
        // rules-channel append, a compile rewrite), only occasionally a
        // real hand edit — so the log must not name a culprit.
        tracing::info!(
            wiki_id = %wiki_id_str,
            source_path = %source_path,
            fact_id = marker.fact_id.as_str(),
            "reindex_file: OFFSETS REPAIRED (standard-wiki region moved on disk)"
        );
    }
    Ok(())
}

async fn apply_orphan_sweep(
    pool: &SqlitePool,
    source_path: &str,
    wiki_id_str: &str,
    active_rows: &[FactIndexRow],
    on_disk_ids: &HashSet<&str>,
    spare_pending: bool,
    report: &mut ReindexFileReport,
) -> Result<()> {
    for row in active_rows {
        if !on_disk_ids.contains(row.fact_id.as_str()) {
            if spare_pending && row.region_start.is_none() {
                // Pending render: offsets are stamped only once the
                // marker lands on disk, so a committed-but-unrendered
                // fact (capture crash window, comment-channel add) is
                // not an orphan — the next compile emits its region.
                continue;
            }
            // Guarded on `source_path`: `active_rows` is a snapshot, and a
            // concurrent promote/compile may have repointed the row to
            // another page after it was taken — a fact that moved away is
            // not an orphan of this page.
            let touched = fact_index::mark_forgotten_at(
                pool,
                &row.fact_id,
                source_path,
                REASON_MARKER_REMOVED,
            )
            .await?;
            if touched > 0 {
                report.orphaned += 1;
                tracing::info!(
                    wiki_id = %wiki_id_str,
                    source_path = %source_path,
                    fact_id = row.fact_id.as_str(),
                    "reindex_file: ORPHANED (marker missing from disk)"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::fact_index::NewFact;
    use crate::wiki::atomic_write;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    /// Mint a `UUIDv7` fact id for the standard-wiki fixtures below.
    ///
    /// Production no longer mints ids for smart-wiki content — sections
    /// are keyed by `(source_path, section_ord)` — so this lives with the
    /// tests that still seed `fact_index` rows by hand.
    fn fresh_fact_id() -> FactId {
        let raw = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
        FactId::parse(&raw.to_string()).expect("Uuid::new_v7 is a valid fact id")
    }

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

    fn write_wiki_meta(abs_dir: &Path, wiki_id: &str) {
        std::fs::create_dir_all(abs_dir).unwrap();
        let slug = wiki_id.rsplit('/').next().unwrap_or(wiki_id);
        let meta = format!(
            "---\nwiki_id: {wiki_id}\ntitle: {slug}\nwiki_type: wiki-user\nslug: {slug}\nparent_wiki_id: null\n---\n",
        );
        atomic_write(&abs_dir.join("_meta.md"), meta.as_bytes()).expect("write meta");
    }

    /// A smart wiki (`smart: true`) — the only family `reindex_full`
    /// section-indexes (content-indexed, markerless).
    fn write_smart_wiki_meta(abs_dir: &Path, wiki_id: &str) {
        write_smart_wiki_meta_with(abs_dir, wiki_id, "");
    }

    /// A smart wiki (`smart: true`) owned by the user whose id matches
    /// `wiki_id` — a `wiki-user` identity root, so the scope-principal
    /// derivation yields `user:<wiki_id>` (the owner the section rows
    /// inherit). Extra `_meta` lines (e.g. `shared_with: [user:bob]`) are
    /// appended verbatim.
    fn write_smart_wiki_meta_with(abs_dir: &Path, wiki_id: &str, extra: &str) {
        std::fs::create_dir_all(abs_dir).unwrap();
        let slug = wiki_id.rsplit('/').next().unwrap_or(wiki_id);
        let meta = format!(
            "---\nwiki_id: {wiki_id}\ntitle: {slug}\nwiki_type: wiki-user\nslug: {slug}\nparent_wiki_id: null\nsmart: true\n{extra}---\n",
        );
        atomic_write(&abs_dir.join("_meta.md"), meta.as_bytes()).expect("write meta");
    }

    fn write_page(abs_dir: &Path, page: &str, body: &str) {
        atomic_write(&abs_dir.join(page), body.as_bytes()).expect("write page");
    }

    /// Seed an alice-owned row directly (the standard family never
    /// creates rows from disk, so standard-wiki tests seed the DB the way
    /// capture / the comment channel do).
    async fn seed_fact(
        pool: &SqlitePool,
        fact_id: &FactId,
        source_path: &str,
        text: &str,
        offsets: Option<(i64, i64)>,
    ) {
        let new = NewFact {
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: "alice".to_owned(),
            source_path: source_path.to_owned(),
            region_start: offsets.map(|(s, _)| s),
            region_end: offsets.map(|(_, e)| e),
            text: text.to_owned(),
            embedding: vec![0.0; 8],
            owner_id: "user:alice".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        fact_index::insert(pool, &new).await.expect("seed fact");
    }

    #[tokio::test]
    async fn strip_fact_region_removes_retired_region_and_resyncs_neighbour() {
        // The disk half of a supersede: after A is retired, its region is
        // excised from the page and the surviving neighbour B's byte offsets
        // are re-synced (B shifted left by the excision).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let a = FactId::parse("018f1234-5678-7abc-9def-00000000aaaa").unwrap();
        let b = FactId::parse("018f1234-5678-7abc-9def-00000000bbbb").unwrap();
        let open_a = String::from("{{f=") + a.as_str() + "}}";
        let open_b = String::from("{{f=") + b.as_str() + "}}";
        let close = "{{/}}";
        let body = format!("intro {open_a}body a{close} mid {open_b}body b{close} end\n");
        write_page(&wiki_dir, "p.md", &body);

        let span = |open: &str| {
            let lo = body.find(open).unwrap();
            let hi = body[lo..].find(close).unwrap() + lo + close.len();
            (i64::try_from(lo).unwrap(), i64::try_from(hi).unwrap())
        };
        seed_fact(&pool, &a, "wikis/alice/p.md", "body a", Some(span(&open_a))).await;
        seed_fact(&pool, &b, "wikis/alice/p.md", "body b", Some(span(&open_b))).await;

        // Retire A first (real order), then strip its region from disk.
        fact_index::mark_superseded(&pool, &a, &b)
            .await
            .expect("supersede a");
        let stripped = strip_fact_region(&pool, &tree, embedder.clone(), &a)
            .await
            .expect("strip");
        assert!(stripped, "A's region was excised");

        let after = std::fs::read_to_string(wiki_dir.join("p.md")).unwrap();
        assert!(!after.contains(&open_a), "A marker removed from disk");
        assert!(after.contains(&open_b), "B marker retained on disk");

        // B is still active and its offsets now bracket its shifted marker.
        let brow = fact_index::find_by_id(&pool, &b)
            .await
            .unwrap()
            .expect("B row");
        assert!(brow.superseded_at.is_none() && brow.deleted_at.is_none());
        let bs = usize::try_from(brow.region_start.unwrap()).unwrap();
        let be = usize::try_from(brow.region_end.unwrap()).unwrap();
        assert!(
            after[bs..be].starts_with(&open_b) && after[bs..be].ends_with(close),
            "B offsets re-synced after the excision"
        );

        // A idempotent second strip is a no-op (its marker is already gone).
        let again = strip_fact_region(&pool, &tree, embedder, &a)
            .await
            .expect("second strip");
        assert!(!again, "second strip finds nothing to remove");
    }

    /// Safety pin: the strip refuses an ACTIVE fact — excising live prose
    /// would hand the fact to the marker-removed orphan sweep. Every
    /// caller relies on this guard to be race-safe against reverts.
    #[tokio::test]
    async fn strip_fact_region_refuses_an_active_fact() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let a = FactId::parse("018f1234-5678-7abc-9def-00000000aaaa").unwrap();
        let open_a = String::from("{{f=") + a.as_str() + "}}";
        let body = format!("intro {open_a}live body{{{{/}}}} end\n");
        write_page(&wiki_dir, "p.md", &body);
        let lo = i64::try_from(body.find(&open_a).unwrap()).unwrap();
        let hi = i64::try_from(body.find("{{/}}").unwrap() + 5).unwrap();
        seed_fact(&pool, &a, "wikis/alice/p.md", "live body", Some((lo, hi))).await;

        let stripped = strip_fact_region(&pool, &tree, embedder, &a)
            .await
            .expect("strip call");
        assert!(!stripped, "an active fact must never be stripped");
        let after = std::fs::read_to_string(wiki_dir.join("p.md")).unwrap();
        assert_eq!(after, body, "page untouched");
    }

    /// The page-level strip re-parses the page, so it excises every
    /// retired region even when the stored offsets went stale, leaves
    /// active regions and their prose intact, and settles the retired
    /// rows' offsets (convergence).
    #[tokio::test]
    async fn page_strip_excises_retired_markers_even_with_stale_offsets() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let a = FactId::parse("018f1234-5678-7abc-9def-00000000aaaa").unwrap();
        let b = FactId::parse("018f1234-5678-7abc-9def-00000000bbbb").unwrap();
        let c = FactId::parse("018f1234-5678-7abc-9def-00000000cccc").unwrap();
        let close = "{{/}}";
        let region = |id: &FactId, body: &str| format!("{{{{f={}}}}}{body}{close}", id.as_str());
        let body = format!(
            "# Rules\n\n- {}\n- {}\n- {}\n",
            region(&a, "old rule"),
            region(&b, "wrong rule"),
            region(&c, "live rule"),
        );
        write_page(&wiki_dir, "rules.md", &body);
        // A: retired, offsets deliberately STALE (point at the wrong span).
        seed_fact(&pool, &a, "wikis/alice/rules.md", "old rule", Some((0, 8))).await;
        // B: retired, offsets never stamped (NULL) — still excised by parse.
        seed_fact(&pool, &b, "wikis/alice/rules.md", "wrong rule", None).await;
        // C: active with correct offsets.
        let span_c = region(&c, "live rule");
        let lo = i64::try_from(body.find(&span_c).unwrap()).unwrap();
        let hi = lo + i64::try_from(span_c.len()).unwrap();
        seed_fact(
            &pool,
            &c,
            "wikis/alice/rules.md",
            "live rule",
            Some((lo, hi)),
        )
        .await;

        let successor = FactId::parse("018f1234-5678-7abc-9def-00000000dddd").unwrap();
        fact_index::mark_superseded(&pool, &a, &successor)
            .await
            .unwrap();
        fact_index::mark_forgotten(&pool, &b, "fact_forget_vote")
            .await
            .unwrap();

        let (stripped, settled) =
            strip_retired_regions_on_page(&pool, &tree, embedder.clone(), "wikis/alice/rules.md")
                .await
                .expect("page strip");
        assert_eq!(stripped, 2, "both retired regions excised");
        assert_eq!(settled, 1, "A's stale offsets settled (B had none)");

        let after = std::fs::read_to_string(wiki_dir.join("rules.md")).unwrap();
        assert!(!after.contains(a.as_str()) && !after.contains("old rule"));
        assert!(!after.contains(b.as_str()) && !after.contains("wrong rule"));
        assert!(after.contains(c.as_str()) && after.contains("live rule"));
        assert!(after.contains("# Rules"), "prose scaffolding preserved");

        let row_a = fact_index::find_by_id(&pool, &a).await.unwrap().unwrap();
        assert!(row_a.region_start.is_none() && row_a.region_end.is_none());

        // Converged: a second pass finds nothing to excise or settle.
        let (again_stripped, again_settled) =
            strip_retired_regions_on_page(&pool, &tree, embedder, "wikis/alice/rules.md")
                .await
                .expect("second page strip");
        assert_eq!((again_stripped, again_settled), (0, 0));
    }

    /// The light-dream hygiene sweep strips residue from NON-plan pages
    /// (`rules.md`) but leaves plan pages to their next compile; once a
    /// page's retired rows are settled it stops being a candidate.
    #[tokio::test]
    async fn sweep_cleans_non_plan_pages_and_skips_plan_pages() {
        use crate::planner::{CompilationPlan, PagePlan, PageType, save_plan};
        use std::collections::BTreeMap;

        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let a = FactId::parse("018f1234-5678-7abc-9def-00000000aaaa").unwrap();
        let b = FactId::parse("018f1234-5678-7abc-9def-00000000bbbb").unwrap();
        let region = |id: &FactId, body: &str| format!("{{{{f={}}}}}{body}{{{{/}}}}", id.as_str());
        let rules_body = format!("# Rules\n\n- {}\n", region(&a, "retired rule"));
        write_page(&wiki_dir, "rules.md", &rules_body);
        let topic_body = format!("# Topic\n\n{}\n", region(&b, "retired claim"));
        write_page(&wiki_dir, "topic.md", &topic_body);

        let span = |page: &str, needle: &str| {
            let lo = page.find(needle).unwrap();
            (
                i64::try_from(lo).unwrap(),
                i64::try_from(lo + needle.len()).unwrap(),
            )
        };
        seed_fact(
            &pool,
            &a,
            "wikis/alice/rules.md",
            "retired rule",
            Some(span(&rules_body, &region(&a, "retired rule"))),
        )
        .await;
        seed_fact(
            &pool,
            &b,
            "wikis/alice/topic.md",
            "retired claim",
            Some(span(&topic_body, &region(&b, "retired claim"))),
        )
        .await;
        fact_index::mark_forgotten(&pool, &a, "fact_forget_vote")
            .await
            .unwrap();
        fact_index::mark_forgotten(&pool, &b, "fact_forget_vote")
            .await
            .unwrap();

        // topic.md is owned by the compilation plan; rules.md is not.
        let mut pages = BTreeMap::new();
        pages.insert(
            "topic".to_owned(),
            PagePlan {
                slug: "topic".to_owned(),
                title: "Topic".to_owned(),
                description: "a plan page".to_owned(),
                style: None,
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "topic.md".to_owned(),
            },
        );
        save_plan(
            &tree,
            &CompilationPlan {
                pages,
                merged_pages: Vec::new(),
                link_graph: BTreeMap::new(),
                compilation_order: vec!["topic".to_owned()],
                generated_at: "2026-07-02T00:00:00Z".to_owned(),
                fact_count: 1,
                dirty_pages: Vec::new(),
                force_dirty: Vec::new(),
                refile_candidates: Vec::new(),
                reopen_pages: Vec::new(),
            },
        )
        .expect("save plan");

        let report = sweep_retired_regions(&pool, &tree, embedder.clone(), 64)
            .await
            .expect("sweep");
        assert_eq!(report.pages_examined, 1, "only the non-plan page opened");
        assert_eq!(report.regions_stripped, 1);
        assert_eq!(report.pages_skipped_plan, 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let rules_after = std::fs::read_to_string(wiki_dir.join("rules.md")).unwrap();
        assert!(!rules_after.contains(a.as_str()), "rules residue excised");
        let topic_after = std::fs::read_to_string(wiki_dir.join("topic.md")).unwrap();
        assert_eq!(
            topic_after, topic_body,
            "plan page untouched (self-cleans at compile)"
        );

        // Convergence: rules.md settled out of the candidate set; the plan
        // page stays a (skipped) candidate until its compile rewrites it.
        let second = sweep_retired_regions(&pool, &tree, embedder, 64)
            .await
            .expect("second sweep");
        assert_eq!(second.pages_examined, 0);
        assert_eq!(second.regions_stripped, 0);
        assert_eq!(second.pages_skipped_plan, 1);
    }

    // ---------- reconcile_wiki_ids (boot safety net) ----------

    /// Seed a fact with an explicit `wiki_id` (the reconcile tests need
    /// rows whose claimed wiki diverges from their `source_path`).
    async fn seed_fact_in(pool: &SqlitePool, fact_id: &FactId, wiki_id: &str, source_path: &str) {
        let new = NewFact {
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: wiki_id.to_owned(),
            source_path: source_path.to_owned(),
            region_start: None,
            region_end: None,
            text: "a claim".to_owned(),
            embedding: vec![0.0; 8],
            owner_id: "user:alice".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        fact_index::insert(pool, &new).await.expect("seed fact");
    }

    #[tokio::test]
    async fn reconcile_wiki_ids_fixes_divergence_by_longest_prefix() {
        let dir = tempdir().unwrap();
        // Nested layout: famiglia + its sub-wiki famiglia-bruno-battaglia.
        write_wiki_meta(&dir.path().join("wikis/famiglia"), "famiglia");
        write_wiki_meta(
            &dir.path().join("wikis/famiglia/bruno-battaglia"),
            "famiglia-bruno-battaglia",
        );
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;

        let divergent = FactId::parse("018f1234-5678-7abc-9def-00000000aaaa").unwrap();
        let nested_ok = FactId::parse("018f1234-5678-7abc-9def-00000000bbbb").unwrap();
        let parent_ok = FactId::parse("018f1234-5678-7abc-9def-00000000cccc").unwrap();
        let stray = FactId::parse("018f1234-5678-7abc-9def-00000000dddd").unwrap();
        let retired = FactId::parse("018f1234-5678-7abc-9def-00000000eeee").unwrap();
        // Divergent: claims the parent, lives under the sub-wiki → the
        // LONGEST prefix must win (a pure-SQL pass could not know this).
        seed_fact_in(
            &pool,
            &divergent,
            "famiglia",
            "wikis/famiglia/bruno-battaglia/dossier.md",
        )
        .await;
        // Consistent rows (one per level) — untouched.
        seed_fact_in(
            &pool,
            &nested_ok,
            "famiglia-bruno-battaglia",
            "wikis/famiglia/bruno-battaglia/index.md",
        )
        .await;
        seed_fact_in(&pool, &parent_ok, "famiglia", "wikis/famiglia/index.md").await;
        // Under no known wiki: left alone (WARN).
        seed_fact_in(&pool, &stray, "famiglia", "trash/famiglia/index.md").await;
        // Retired + divergent: not an ACTIVE row, so out of scope.
        seed_fact_in(
            &pool,
            &retired,
            "famiglia",
            "wikis/famiglia/bruno-battaglia/old.md",
        )
        .await;
        fact_index::mark_forgotten(&pool, &retired, "user_request")
            .await
            .unwrap();

        let report = reconcile_wiki_ids(&pool, &tree).await.expect("reconcile");
        assert_eq!(report.scanned, 4, "active rows only");
        assert_eq!(report.fixed, 1);
        assert_eq!(report.unknown, 1);

        let fixed = fact_index::find_by_id(&pool, &divergent)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fixed.wiki_id, "famiglia-bruno-battaglia",
            "longest directory prefix wins"
        );
        for (id, expected) in [
            (&nested_ok, "famiglia-bruno-battaglia"),
            (&parent_ok, "famiglia"),
            (&stray, "famiglia"),
            (&retired, "famiglia"),
        ] {
            let row = fact_index::find_by_id(&pool, id).await.unwrap().unwrap();
            assert_eq!(row.wiki_id, expected, "{id} must be untouched");
        }

        // Idempotent: a second pass fixes nothing.
        let again = reconcile_wiki_ids(&pool, &tree).await.expect("second pass");
        assert_eq!(again.fixed, 0);
        assert_eq!(again.unknown, 1, "the stray keeps being reported");
    }

    #[tokio::test]
    async fn embedder_identity_stamps_then_matches_then_detects_model_mismatch() {
        let pool = make_pool().await;
        let bge = FakeEmbedder::new("bge-m3", 1024);

        // Fresh store → first check stamps the identity.
        assert_eq!(
            check_embedder_identity(&pool, &bge).await.unwrap(),
            EmbedderIdentity::Stamped
        );
        // Same embedder now matches.
        assert_eq!(
            check_embedder_identity(&pool, &bge).await.unwrap(),
            EmbedderIdentity::Match
        );
        // A different model (same dim) is reported, not stamped over.
        let other = FakeEmbedder::new("gte-large", 1024);
        match check_embedder_identity(&pool, &other).await.unwrap() {
            EmbedderIdentity::Mismatch {
                stored_model,
                configured_model,
                ..
            } => {
                assert_eq!(stored_model, "bge-m3");
                assert_eq!(configured_model, "gte-large");
            },
            other => panic!("expected mismatch, got {other:?}"),
        }
        // The recorded identity is unchanged after a reported mismatch.
        assert_eq!(
            check_embedder_identity(&pool, &bge).await.unwrap(),
            EmbedderIdentity::Match
        );
    }

    #[tokio::test]
    async fn embedder_identity_flags_dim_mismatch_without_recorded_identity() {
        let pool = make_pool().await;
        // A pre-guard store: a fact with a dim-8 vector, no recorded identity.
        seed_fact(
            &pool,
            &fresh_fact_id(),
            "alice/index.md",
            "ciao",
            Some((0, 4)),
        )
        .await;

        // A 4-dim embedder must be flagged (a dim change breaks cosine),
        // not stamped over the existing vectors.
        let four = FakeEmbedder::new("tiny", 4);
        match check_embedder_identity(&pool, &four).await.unwrap() {
            EmbedderIdentity::Mismatch {
                stored_model,
                stored_dim,
                configured_dim,
                ..
            } => {
                assert_eq!(stored_model, "(unknown)");
                assert_eq!(stored_dim, 8);
                assert_eq!(configured_dim, 4);
            },
            other => panic!("expected dim mismatch, got {other:?}"),
        }
        // A matching-dimension embedder on the same store stamps cleanly.
        let eight = FakeEmbedder::new("dim8", 8);
        assert_eq!(
            check_embedder_identity(&pool, &eight).await.unwrap(),
            EmbedderIdentity::Stamped
        );
    }

    #[tokio::test]
    async fn reindex_file_section_indexes_smart_page() {
        // Smart wiki: plain markdown, no markers. Each heading-delimited
        // section becomes one row of `wiki_sections`, keyed by its
        // position — and NOT a `fact_index` row, which is the standard
        // family's authoritative fact store.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let body = "# Project\n\nIntro paragraph.\n\n## Auth\n\nAuth uses JWT.\n";
        let page = wiki_dir.join("design.md");
        write_page(&wiki_dir, "design.md", body);

        let report = reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .expect("reindex");
        assert_eq!(report.inserted, 2, "two sections → two rows");
        assert_eq!(report.updated, 0);
        assert_eq!(report.wiki_id.as_deref(), Some("alice"));

        let rows = sections::find_page_sections(&pool, "wikis/alice/design.md")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for (ord, row) in rows.iter().enumerate() {
            assert_eq!(row.section_ord, i64::try_from(ord).unwrap());
            assert_eq!(row.wiki_id, "alice");
            assert_eq!(row.embedding_dim, 8);
        }
        // The heading words are part of the indexed text, and the chain is
        // kept alongside it so the dashboard can label the section.
        assert!(
            rows.iter()
                .any(|r| r.text.contains("Auth") && r.text.contains("JWT"))
        );
        assert!(rows.iter().any(|r| {
            r.heading_path
                .as_deref()
                .is_some_and(|h| h.contains("Auth"))
        }));

        // Smart content never lands in the fact store.
        assert!(
            fact_index::find_active_by_source_path(&pool, "wikis/alice/design.md")
                .await
                .unwrap()
                .is_empty(),
            "smart sections must not create fact_index rows"
        );
    }

    #[tokio::test]
    async fn reindex_file_smart_caps_section_length() {
        // Roadmap 48h. A section is a retrieval unit: it is quoted whole
        // into a bounded recall slot, and the slot always admits its first
        // hit whatever the size — so an uncapped section swallows the
        // budget and starves every other hit. The smart indexer therefore
        // chunks with its own policy, not the wider document-ingest one.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        // Two shapes, both of which reached production oversized:
        // (1) a heading, a blank line, one unbroken paragraph; (2) a
        // heading whose body starts on the NEXT LINE — a changelog entry,
        // a table, a dense list — which makes heading and body a single
        // paragraph. Shape (2) used to bypass the cap entirely: the
        // heading branch pushed its trailing lines into the buffer
        // without splitting them, which is how a 6 994-char section got
        // indexed.
        let body = format!(
            "# Log\n\n{}\n\n## Changelog\n{}\n",
            "parola ".repeat(1_500),
            "voce di changelog molto lunga\n".repeat(300),
        );
        let page = wiki_dir.join("log.md");
        write_page(&wiki_dir, "log.md", &body);

        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .expect("reindex");

        let rows = sections::find_page_sections(&pool, "wikis/alice/log.md")
            .await
            .unwrap();
        assert!(rows.len() > 1, "an oversized paragraph must be split");
        assert!(
            rows.iter().any(|r| r
                .heading_path
                .as_deref()
                .is_some_and(|h| h.ends_with("Changelog"))),
            "the heading-attached body must be indexed under its heading"
        );
        // The heading path is prefixed to the body, hence the slack.
        let slack = "Log › Changelog\n\n".chars().count();
        for row in &rows {
            assert!(
                row.text.chars().count() <= crate::document::SECTION_MAX_CHARS + slack,
                "section {} is {} chars, over the index-time cap",
                row.section_ord,
                row.text.chars().count()
            );
        }
        // And the cap leaves room for a second hit in the recall slot.
        assert!(
            crate::document::SECTION_MAX_CHARS + slack
                < crate::ingest::IngestPolicy::default().project_docs_char_budget,
            "a maximal section must not fill the project-docs slot on its own"
        );
    }

    #[tokio::test]
    async fn reindex_file_smart_dedups_identical_sections() {
        // Regression (report #5 / roadmap 26): a smart page with two
        // identical sections must index to ONE row, not two — else the same
        // block comes back twice in wiki_navigate's flat hits (same text,
        // same score, distinct fact_id).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        // Two sections with identical heading + body.
        let body = "# Note\n\nSame content.\n\n# Note\n\nSame content.\n";
        let page = wiki_dir.join("dup.md");
        write_page(&wiki_dir, "dup.md", body);

        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .expect("reindex");

        let rows = sections::find_page_sections(&pool, "wikis/alice/dup.md")
            .await
            .unwrap();
        let distinct: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            rows.len(),
            distinct.len(),
            "a page must never index the same section text to two rows"
        );
        assert_eq!(rows.len(), 1, "two identical sections collapse to one row");
    }

    #[tokio::test]
    async fn reindex_file_smart_self_heals_stale_tail_sections() {
        // A page whose stored sections outnumber what is on disk (a
        // shrunk page whose reindex was interrupted, or a pre-migration
        // leftover) converges on the next pass: the tail positions are
        // dropped. Duplicate *rows* are no longer expressible at all —
        // `(source_path, section_ord)` is the primary key — so the race
        // this used to guard against cannot recur.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let page = wiki_dir.join("p.md");
        write_page(&wiki_dir, "p.md", "Single section body.\n");

        // Seed a stale two-section state for a page that now holds one.
        sections::replace_page_sections(
            &pool,
            "wikis/alice/p.md",
            &[
                sections::NewSection {
                    wiki_id: "alice".to_owned(),
                    source_path: "wikis/alice/p.md".to_owned(),
                    section_ord: 0,
                    heading_path: None,
                    text: "Single section body.".to_owned(),
                    embedding: vec![0.0; 8],
                },
                sections::NewSection {
                    wiki_id: "alice".to_owned(),
                    source_path: "wikis/alice/p.md".to_owned(),
                    section_ord: 1,
                    heading_path: None,
                    text: "stale leftover".to_owned(),
                    embedding: vec![0.0; 8],
                },
            ],
        )
        .await
        .unwrap();

        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .expect("reindex");

        let rows = sections::find_page_sections(&pool, "wikis/alice/p.md")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "the stale tail position is dropped");
        assert!(rows[0].text.contains("Single section body"));
    }

    #[tokio::test]
    async fn reindex_file_smart_is_idempotent_on_unchanged_page() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let body = "# Topic\n\nFirst.\n\n## Sub\n\nSecond.\n";
        let page = wiki_dir.join("p.md");
        write_page(&wiki_dir, "p.md", body);

        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        let report = reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert_eq!(report.inserted, 0, "unchanged page mutates zero rows");
        assert_eq!(report.updated, 0);
        assert_eq!(report.orphaned, 0);
    }

    #[tokio::test]
    async fn reindex_file_smart_resections_on_edit() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let page = wiki_dir.join("p.md");
        write_page(&wiki_dir, "p.md", "# A\n\nold body.\n");
        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();

        write_page(&wiki_dir, "p.md", "# A\n\nnew body.\n");
        let report = reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert!(report.changed(), "edited page is re-sectioned");

        let rows = sections::find_page_sections(&pool, "wikis/alice/p.md")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].text.contains("new body"));
        assert!(!rows[0].text.contains("old body"));
    }

    #[tokio::test]
    async fn smart_registry_holds_the_wiki_level_acl_not_the_sections() {
        // The wiki-level ACL lives in `_meta` and is projected ONCE into
        // `smart_wikis`. Sections carry no ACL of their own — which is
        // what makes a sharing edit a one-row write instead of one write
        // per indexed section.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta_with(&wiki_dir, "alice", "shared_with: [user:bob]\n");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let page = wiki_dir.join("p.md");
        write_page(&wiki_dir, "p.md", "# A\n\nshared content.\n");
        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        project_smart_wiki_registry(&pool, &tree).await.unwrap();

        let rows = sections::find_page_sections(&pool, "wikis/alice/p.md")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        let registered = sections::find_smart_wiki(&pool, "alice")
            .await
            .unwrap()
            .expect("smart wiki projected");
        assert_eq!(registered.owner_id, "user:alice".parse().unwrap());
        assert_eq!(registered.shared_with, vec!["user:bob".parse().unwrap()]);
    }

    #[tokio::test]
    async fn smart_registry_drops_a_wiki_that_stopped_being_smart() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;

        let report = project_smart_wiki_registry(&pool, &tree).await.unwrap();
        assert_eq!(report.projected, 1);
        assert!(
            sections::find_smart_wiki(&pool, "alice")
                .await
                .unwrap()
                .is_some()
        );

        // The operator flips the flag off by hand in `_meta.md` — the
        // file is the source of truth, so the next projection follows it.
        write_wiki_meta(&wiki_dir, "alice");
        let tree = WikiTree::open(dir.path()).expect("reopen tree");
        let report = project_smart_wiki_registry(&pool, &tree).await.unwrap();
        assert_eq!(report.projected, 0);
        assert_eq!(report.removed, 1);
        assert!(
            sections::find_smart_wiki(&pool, "alice")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn backfill_moves_legacy_smart_rows_and_is_idempotent() {
        // The one-time migration tail: smart-wiki content that still lives
        // in `fact_index` is moved into `wiki_sections` with its embedding
        // copied verbatim, and the legacy rows are dropped.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");
        write_page(&wiki_dir, "p.md", "# A\n\nlegacy body.\n");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;

        let id1 = FactId::parse("018f1234-5678-7abc-9def-00000000e001").unwrap();
        let id2 = FactId::parse("018f1234-5678-7abc-9def-00000000e002").unwrap();
        seed_fact(
            &pool,
            &id1,
            "wikis/alice/p.md",
            "first legacy section",
            None,
        )
        .await;
        seed_fact(
            &pool,
            &id2,
            "wikis/alice/p.md",
            "second legacy section",
            None,
        )
        .await;

        let report = backfill_smart_sections(&pool, &tree).await.unwrap();
        assert_eq!(report.pages_moved, 1);
        assert_eq!(report.sections_written, 2);
        assert_eq!(report.legacy_rows_dropped, 2);

        let moved = sections::find_page_sections(&pool, "wikis/alice/p.md")
            .await
            .unwrap();
        assert_eq!(moved.len(), 2);
        // UUIDv7 is time-ordered, so insertion order is reconstructed.
        assert_eq!(moved[0].section_ord, 0);
        assert!(moved[0].text.contains("first legacy"));
        assert!(moved[1].text.contains("second legacy"));
        assert!(
            fact_index::find_active_by_source_path(&pool, "wikis/alice/p.md")
                .await
                .unwrap()
                .is_empty(),
            "legacy rows are gone from the fact store"
        );

        // Re-running on an already-migrated store changes nothing.
        let again = backfill_smart_sections(&pool, &tree).await.unwrap();
        assert_eq!(again, SmartBackfillReport::default());
        assert_eq!(
            sections::find_page_sections(&pool, "wikis/alice/p.md")
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn standard_reindex_never_creates_rows_from_disk_markers() {
        // Standard wiki: the DB is the authoritative fact store, so a
        // disk marker with no row — even a legacy full marker carrying
        // inline ACL — is stale render residue, never a fresh fact.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let f_full = fresh_fact_id();
        let f_bare = fresh_fact_id();
        let body = format!(
            "{{{{owner=global f={f1}}}}}pasted{{{{/}}}}\n{{{{f={f2}}}}}residue{{{{/}}}}\n",
            f1 = f_full.as_str(),
            f2 = f_bare.as_str(),
        );
        let page = wiki_dir.join("intro.md");
        write_page(&wiki_dir, "intro.md", &body);

        let report = reindex_file(&pool, &tree, embedder, &page).await.unwrap();
        assert_eq!(report.inserted, 0);
        assert!(
            fact_index::find_by_id(&pool, &f_full)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fact_index::find_by_id(&pool, &f_bare)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn standard_reindex_repairs_offsets_without_touching_text() {
        // Standard wiki: a hand edit moved the region. The offsets are
        // repaired; the row's canonical claim text is NOT overwritten by
        // the prose span (they are different strings by design).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let fact_id = fresh_fact_id();
        let body = format!(
            "Some prose the operator added above.\n{{{{f={f}}}}}she loves hiking these days{{{{/}}}}\n",
            f = fact_id.as_str()
        );
        let page = wiki_dir.join("intro.md");
        write_page(&wiki_dir, "intro.md", &body);
        // Row claims stale offsets (pre-edit) and canonical claim text.
        seed_fact(
            &pool,
            &fact_id,
            "wikis/alice/intro.md",
            "Alice loves hiking",
            Some((0, 10)),
        )
        .await;

        let report = reindex_file(&pool, &tree, embedder, &page).await.unwrap();
        assert_eq!(report.updated, 1);
        assert_eq!(report.inserted, 0);
        assert_eq!(report.orphaned, 0);

        let row = fact_index::find_by_id(&pool, &fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.text, "Alice loves hiking",
            "standard-wiki claim text is DB-authoritative — never rewritten from the prose span"
        );
        let start = usize::try_from(row.region_start.expect("offsets repaired")).unwrap();
        let end = usize::try_from(row.region_end.expect("offsets repaired")).unwrap();
        let raw = std::fs::read_to_string(&page).unwrap();
        assert!(raw[start..end].contains("she loves hiking these days"));
    }

    #[tokio::test]
    async fn standard_sweep_spares_pending_render_rows() {
        // Two committed rows point at the page; only one was ever
        // rendered (has offsets). The rendered one lost its marker to a
        // hand edit → forgotten (the operator's gesture). The offset-less
        // one is a pending render (capture crash window, comment-channel
        // add) → spared.
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let f_rendered = fresh_fact_id();
        let f_pending = fresh_fact_id();
        seed_fact(
            &pool,
            &f_rendered,
            "wikis/alice/intro.md",
            "rendered claim",
            Some((0, 30)),
        )
        .await;
        seed_fact(
            &pool,
            &f_pending,
            "wikis/alice/intro.md",
            "pending claim",
            None,
        )
        .await;
        // Neither marker is on disk.
        write_page(&wiki_dir, "intro.md", "hand-rewritten prose, no markers\n");

        let report = reindex_file(&pool, &tree, embedder, &wiki_dir.join("intro.md"))
            .await
            .unwrap();
        assert_eq!(report.orphaned, 1);

        let rendered = fact_index::find_by_id(&pool, &f_rendered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rendered.deleted_reason.as_deref(),
            Some(REASON_MARKER_REMOVED)
        );
        let pending = fact_index::find_by_id(&pool, &f_pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pending.deleted_at.is_none(),
            "a pending render is not an orphan"
        );
    }

    #[tokio::test]
    async fn standard_file_delete_spares_pending_render_rows() {
        // Page deletion is the forget gesture for what the operator
        // could see: rendered rows are tombstoned, pending renders are
        // not (their prose never existed in the deleted file).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let f_rendered = fresh_fact_id();
        let f_pending = fresh_fact_id();
        seed_fact(
            &pool,
            &f_rendered,
            "wikis/alice/intro.md",
            "rendered claim",
            Some((0, 30)),
        )
        .await;
        seed_fact(
            &pool,
            &f_pending,
            "wikis/alice/intro.md",
            "pending claim",
            None,
        )
        .await;
        // The page never gets written / was removed: reindex the missing path.
        let report = reindex_file(&pool, &tree, embedder, &wiki_dir.join("intro.md"))
            .await
            .unwrap();
        assert!(report.file_missing);
        assert_eq!(report.orphaned, 1);

        let rendered = fact_index::find_by_id(&pool, &f_rendered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rendered.deleted_reason.as_deref(),
            Some(REASON_FILE_REMOVED)
        );
        let pending = fact_index::find_by_id(&pool, &f_pending)
            .await
            .unwrap()
            .unwrap();
        assert!(pending.deleted_at.is_none());
    }

    #[tokio::test]
    async fn reindex_file_smart_drops_removed_section() {
        // Smart wiki: deleting a section from the page removes its row on
        // the next reindex (drop-and-reinsert — no tombstone).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let page = wiki_dir.join("intro.md");
        write_page(
            &wiki_dir,
            "intro.md",
            "# One\n\nfirst.\n\n## Two\n\nsecond.\n",
        );
        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert_eq!(
            sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .unwrap()
                .len(),
            2
        );

        // Drop the second section on disk.
        write_page(&wiki_dir, "intro.md", "# One\n\nfirst.\n");
        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        let rows = sections::find_page_sections(&pool, "wikis/alice/intro.md")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "removed section's row is gone");
        assert!(rows[0].text.contains("first"));
        assert!(!rows.iter().any(|r| r.text.contains("second")));
    }

    #[tokio::test]
    async fn reindex_file_smart_hard_drops_rows_when_file_deleted() {
        // Markerless smart wiki: a deleted page's section rows are
        // hard-dropped (no tombstone).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let page = wiki_dir.join("intro.md");
        write_page(&wiki_dir, "intro.md", "# Lonely\n\njust one.\n");
        reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert_eq!(
            sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .unwrap()
                .len(),
            1
        );

        std::fs::remove_file(&page).unwrap();
        let report = reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert!(report.file_missing);
        assert_eq!(report.orphaned, 1);

        // Hard delete: the sections are gone entirely, not tombstoned —
        // the page was their only source.
        assert!(
            sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .unwrap()
                .is_empty(),
            "smart page delete is a hard delete, no tombstone"
        );
    }

    #[tokio::test]
    async fn reindex_file_ignores_marker_without_fact_id() {
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_wiki_meta(&wiki_dir, "alice");

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        // ACL-only wrapper, no `f=` attribute.
        let body = "{{owner=user:alice allow=user:bob}}wrapper body{{/}}\n";
        let page = wiki_dir.join("intro.md");
        write_page(&wiki_dir, "intro.md", body);
        let report = reindex_file(&pool, &tree, embedder.clone(), &page)
            .await
            .unwrap();
        assert_eq!(report.inserted, 0);
        assert_eq!(report.updated, 0);
        assert_eq!(report.orphaned, 0);
    }

    #[tokio::test]
    async fn reindex_file_rejects_path_outside_workdir() {
        let dir = tempdir().unwrap();
        write_wiki_meta(&dir.path().join("wikis/alice"), "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        let stranger = tempdir().unwrap();
        let foreign = stranger.path().join("intro.md");
        std::fs::write(&foreign, "irrelevant").unwrap();

        let err = reindex_file(&pool, &tree, embedder, &foreign)
            .await
            .unwrap_err();
        assert!(matches!(err, ReindexError::PathOutsideTree(_)));
    }

    #[tokio::test]
    async fn reindex_full_section_indexes_smart_skips_standard() {
        // `reindex_full` section-indexes SMART wikis (content-indexed) but
        // SKIPS standard wikis — their fact_index is owned by the
        // buffer→promote→compile chain, so re-reading page markers would
        // overwrite the canonical claim text with compiled prose.
        let dir = tempdir().unwrap();
        write_smart_wiki_meta(&dir.path().join("wikis/acme"), "acme");
        write_wiki_meta(&dir.path().join("wikis/alice"), "alice");
        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));

        // Smart wiki: plain markdown content.
        write_page(
            &dir.path().join("wikis/acme"),
            "intro.md",
            "# Note\n\nsmart content.\n",
        );
        // Standard wiki: a rendered marker the compiler chain owns.
        let f_narr = fresh_fact_id();
        let body_narr = format!(
            "{{{{owner=user:alice f={f}}}}}standard-wiki note{{{{/}}}}\n",
            f = f_narr.as_str()
        );
        write_page(&dir.path().join("wikis/alice"), "intro.md", &body_narr);

        let report = reindex_full(&pool, &tree, embedder).await.unwrap();
        // The smart wiki's section is indexed; the standard wiki is skipped.
        assert_eq!(report.total_inserted, 1);
        assert_eq!(
            sections::find_page_sections(&pool, "wikis/acme/intro.md")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            fact_index::find_by_id(&pool, &f_narr)
                .await
                .unwrap()
                .is_none()
        );
        // The sweep also refreshed the registry for the smart wiki only.
        assert!(
            sections::find_smart_wiki(&pool, "acme")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            sections::find_smart_wiki(&pool, "alice")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn reindex_skips_reserved_briefing_page() {
        // `_briefing.md` is the smart consumer's feedback INBOX, not knowledge
        // it authored — the section-indexer must never turn it into recallable
        // facts (the real KNOWLEDGE page is indexed normally).
        let dir = tempdir().unwrap();
        let wiki_dir = dir.path().join("wikis/alice");
        write_smart_wiki_meta(&wiki_dir, "alice");
        write_page(&wiki_dir, "intro.md", "# Intro\n\nreal content.\n");
        write_page(
            &wiki_dir,
            "_briefing.md",
            "# Session briefing\n\nfeedback item for the consumer.\n",
        );

        let tree = WikiTree::open(dir.path()).expect("open tree");
        let pool = make_pool().await;
        let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
        reindex_full(&pool, &tree, embedder).await.unwrap();

        assert_eq!(
            sections::find_page_sections(&pool, "wikis/alice/intro.md")
                .await
                .unwrap()
                .len(),
            1,
            "the knowledge page is indexed"
        );
        assert!(
            sections::find_page_sections(&pool, "wikis/alice/_briefing.md")
                .await
                .unwrap()
                .is_empty(),
            "_briefing.md (the consumer's inbox) must never be indexed"
        );
    }

    #[tokio::test]
    async fn pick_wiki_for_path_prefers_deeper_wiki() {
        let dir = tempdir().unwrap();
        write_wiki_meta(&dir.path().join("wikis/alice"), "alice");
        write_wiki_meta(&dir.path().join("wikis/alice/sub"), "alice-sub");
        let tree = WikiTree::open(dir.path()).expect("open tree");

        let discovered = tree.walk().unwrap();
        let pick = pick_wiki_for_path(&discovered, &dir.path().join("wikis/alice/sub/intro.md"))
            .expect("pick");
        assert_eq!(pick.meta.wiki_id.as_str(), "alice-sub");

        let pick_parent = pick_wiki_for_path(&discovered, &dir.path().join("wikis/alice/intro.md"))
            .expect("pick parent");
        assert_eq!(pick_parent.meta.wiki_id.as_str(), "alice");
    }
}
