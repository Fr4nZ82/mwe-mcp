// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-kind apply / revert logic for the `wiki_promote` structure
//! proposal kind.
//!
//! The chassis in [`crate::proposals`] dispatches here when a proposal
//! row carries `kind = "wiki_promote"`. This module is responsible for
//! the concrete filesystem + DB work; the chassis owns the state
//! transitions, the `revert_token`, and the deadline.
//!
//! ## Three variants
//!
//! The `wiki_promote` kind covers the structural verbs of the
//! auto-promotion / consolidation pipeline:
//!
//! - **paragraph → file** (default): move N facts from one page of a
//!   wiki to another page of the **same wiki**. The wiki itself is not
//!   created or destroyed. Source and target both live at
//!   `<wikis>/<wiki_id>/*.md`. Selected via `answers.target_page` (no
//!   explicit variant discriminator — this is the default).
//!
//! - **file → sub-wiki**: take an entire page of a wiki and turn it
//!   into a new dedicated sub-wiki whose `index.md` carries the page's
//!   content verbatim. The new wiki id is derived as `parent-childslug`
//!   ([`WikiId::child_of`]); the new directory lives at
//!   `<parent_abs_dir>/<childslug>/`. Selected via
//!   `answers.variant = "file_to_subwiki"`.
//!
//! - **page merge**: move **every** active fact of one concept page (the
//!   husk) onto a near-synonym survivor page of the same wiki, delete the
//!   husk file, and re-home the move in the persisted compilation plan —
//!   the cure front of semantic page consolidation
//!   ([rem-cycle.md §Page-merge sub-job](../../../wiki/design-notes/rem-cycle.md#page-merge-sub-job-semantic-page-consolidation)).
//!   Selected via `answers.variant = "page_merge"`. The revert recreates
//!   the husk from the shell stored in the spec.
//!
//! All variants preserve fact ids verbatim — the same marker on disk
//! and the same row in `fact_index` keep their UUID across the move;
//! what changes is the row's `source_path` (always) and `wiki_id`
//! (only for the file → sub-wiki variant).
//!
//! ## Cross-link rewriting
//!
//! The [narrative compiler](../../../wiki/design-notes/narrative-compiler.md)
//! calls for rewriting cross-link text when a `wiki_promote` ends up changing
//! the parts of the path the wikilink syntax depends on. The
//! paragraph → file variant keeps the wiki id intact, so no
//! cross-link rewriting is required. The file → sub-wiki variant
//! changes the wiki id (a new sub-wiki appears under the parent), but
//! the typical case — promoting `alice/giardinaggio.md` to the new
//! sub-wiki `alice/giardinaggio/` — keeps `[[alice/giardinaggio]]`
//! valid because the parent + slug pair is unchanged; the link text
//! now resolves to the sub-wiki's `index.md` instead of the deleted
//! page, and that index carries the same content. An automatic
//! cross-link rewriter that scans every `.md` file for ambiguous
//! cases (different slug, multiple links per file, links inside
//! markers vs prose, alias-bearing `[[A|display]]` form) is deferred
//! to a separate milestone; this handler emits a log line per moved
//! fact so an operator can grep `wiki_lint` output if cross-links
//! diverge.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::capture_buffer;
use crate::fact_index;
use crate::parser::{self, ParseEvent};
use crate::proposals::{self, ApplyError, EmitParams, ProposalsError, RevertError, kind};
use crate::types::{FactId, Principal, WikiId, WikiSlug};
use crate::wiki::{self, WikiMeta, WikiTree, atomic_write, is_safe_page_path};

// ---------- Request shapes ----------

/// Context fields the chassis loads from `structure_proposals.context`
/// for a `wiki_promote` proposal. Emitter responsibility (the REM
/// auto-promotion path) to populate before insertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromoteContext {
    /// Wiki id whose page the facts currently live in.
    source_wiki_id: String,
    /// Page path within the wiki (relative, e.g. `index.md`).
    source_page: String,
    /// Facts to move. Order is preserved when assembling the target page.
    fact_ids: Vec<String>,
}

/// Answer fields the chassis loads from `structure_proposals.answers`
/// once the user has confirmed via the dashboard. Future variants will
/// add a discriminator field (paragraph-to-file vs file-to-sub-wiki);
/// today only the paragraph-to-file shape is recognised.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromoteAnswers {
    /// Page path within the source wiki to append the regions to.
    /// Created if it does not exist.
    target_page: String,
}

/// One row in the `spec.moved_facts` array — what the chassis writes
/// to the proposal row's `spec` column for the revert path to consume.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MovedFactRecord {
    fact_id: String,
    /// Byte offset of the region in the source page **before** the move.
    old_region_start: i64,
    /// Byte offset one past the region in the source page **before**.
    old_region_end: i64,
    /// Byte offset of the region in the target page **after** the move.
    new_region_start: i64,
    /// Byte offset one past the region in the target page **after**.
    new_region_end: i64,
}

/// `spec` payload written to the proposal row on a successful apply.
/// Read back by [`revert_paragraph_to_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromoteSpec {
    /// Variant discriminator. Stable across schema evolution: future
    /// variants add new values, never reuse this string.
    variant: String,
    source_wiki_id: String,
    source_page: String,
    target_page: String,
    /// `true` iff the target page existed before the apply (revert keeps
    /// the user's prior target content untouched in that case).
    target_existed_before: bool,
    moved_facts: Vec<MovedFactRecord>,
}

/// `spec` payload of a successful cross-wiki single-fact refile. Read
/// back by [`revert_fact_refile`]: the source/dest identities let the
/// revert repoint `wiki_id` back to the source + restore the prose, and
/// `moved` carries the same `(old/new offset)` record the
/// paragraph-to-file variant uses (reused verbatim for one fact).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FactRefileSpec {
    /// Variant discriminator (`fact_refile`).
    variant: String,
    /// Wiki the fact lived in before the refile.
    source_wiki_id: String,
    /// Page (wiki-relative) the fact lived on before the refile.
    source_page: String,
    /// Wiki the fact moved to.
    dest_wiki_id: String,
    /// Page (wiki-relative) the fact moved to.
    dest_page: String,
    /// `true` iff the destination page existed before the apply.
    target_existed_before: bool,
    /// The single moved fact's offset record.
    moved: MovedFactRecord,
}

const VARIANT_PARAGRAPH_TO_FILE: &str = "paragraph_to_file";
const VARIANT_FILE_TO_SUBWIKI: &str = "file_to_subwiki";
const VARIANT_PAGE_MERGE: &str = "page_merge";
const VARIANT_FACT_REFILE: &str = "fact_refile";
const VARIANT_VALIDITY_CLOSE: &str = "validity_close";
const VARIANT_VALIDITY_EDIT: &str = "validity_edit";
const VARIANT_ACL_CHANGE: &str = "acl_change";

/// `wiki_type` label stamped on the new sub-wiki created by the
/// file → sub-wiki variant. Since the type registry + templates were
/// dropped, this is a bare string label, not a registered type — no
/// gate reads it semantically (the smart/standard gates moved to
/// the `_meta` smart flag). A future emergence redesign will
/// rework how emerged wikis are labelled; until then a generic
/// placeholder keeps `WikiMeta.wiki_type` populated.
const DEFAULT_NEW_SUBWIKI_TYPE: &str = "wiki-tech";

// ---------- Variant routers ----------

/// Public entry point for the chassis. Dispatches to the right variant
/// based on `answers.variant`. When `variant` is absent the default is
/// [`VARIANT_PARAGRAPH_TO_FILE`].
///
/// # Errors
///
/// All failure modes funnel into [`ApplyError`].
pub(crate) async fn apply_wiki_promote(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    answers: &Value,
) -> Result<Value, ApplyError> {
    let variant = answers
        .get("variant")
        .and_then(Value::as_str)
        .unwrap_or(VARIANT_PARAGRAPH_TO_FILE);
    match variant {
        VARIANT_PARAGRAPH_TO_FILE => apply_paragraph_to_file(pool, tree, context, answers).await,
        VARIANT_FILE_TO_SUBWIKI => apply_file_to_subwiki(pool, tree, context, answers).await,
        VARIANT_PAGE_MERGE => apply_page_merge(pool, tree, context, answers).await,
        // Closures are applied by the ingest orchestrator before the
        // receipt exists (born-applied); a pending row of this variant
        // cannot occur, so the chassis apply path refuses it loudly.
        VARIANT_VALIDITY_CLOSE => Err(ApplyError::InvalidPayload(
            "validity_close receipts are born applied at ingest; no chassis apply path".into(),
        )),
        // Like validity_close, the validity-edit and acl-change verbs are
        // born-applied by the ingest orchestrator before any receipt
        // exists; a pending row of either variant cannot occur, so the
        // chassis apply path refuses it loudly.
        VARIANT_VALIDITY_EDIT => Err(ApplyError::InvalidPayload(
            "validity_edit receipts are born applied at ingest; no chassis apply path".into(),
        )),
        VARIANT_ACL_CHANGE => Err(ApplyError::InvalidPayload(
            "acl_change receipts are born applied at ingest; no chassis apply path".into(),
        )),
        // Cross-wiki single-fact refile is born-applied by the REM
        // refile sweep before any receipt exists (like the closures); a
        // pending row of this variant cannot occur, so the chassis apply
        // path refuses it loudly.
        VARIANT_FACT_REFILE => Err(ApplyError::InvalidPayload(
            "fact_refile receipts are born applied by REM; no chassis apply path".into(),
        )),
        other => Err(ApplyError::InvalidPayload(format!(
            "unknown wiki_promote variant: {other}",
        ))),
    }
}

/// Public revert entry point for the chassis. Dispatches on the
/// `variant` carried in the stored spec.
///
/// # Errors
///
/// All failure modes funnel into [`RevertError`].
pub(crate) async fn revert_wiki_promote(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let variant = spec
        .get("variant")
        .and_then(Value::as_str)
        .unwrap_or(VARIANT_PARAGRAPH_TO_FILE);
    match variant {
        VARIANT_PARAGRAPH_TO_FILE => revert_paragraph_to_file(pool, tree, spec).await,
        VARIANT_FILE_TO_SUBWIKI => revert_file_to_subwiki(pool, tree, spec).await,
        VARIANT_PAGE_MERGE => revert_page_merge(pool, tree, spec).await,
        VARIANT_VALIDITY_CLOSE => revert_validity_close(pool, spec).await,
        VARIANT_VALIDITY_EDIT => revert_validity_edit(pool, spec).await,
        VARIANT_ACL_CHANGE => revert_acl_change(pool, spec).await,
        VARIANT_FACT_REFILE => revert_fact_refile(pool, tree, spec).await,
        other => Err(RevertError::InvalidPayload(format!(
            "unknown wiki_promote variant: {other}",
        ))),
    }
}

// ---------- Apply ----------

/// Apply a `wiki_promote` proposal: move N regions from `source_page`
/// to `target_page` within the same wiki.
///
/// Steps:
///
/// 1. Parse + validate `context` and `answers`. Both pages must pass
///    the wiki's `is_safe_page_path` check, and the two must differ.
/// 2. Locate the wiki via the [`WikiTree`].
/// 3. Validate every requested `fact_id` is active in `fact_index` and
///    its stored `source_path` matches the workdir-relative path of
///    `<wiki>/<source_page>`.
/// 4. Read the source page from disk, parse it, and locate each
///    requested region (matched by `fact_id` on the parser's marker).
///    Any missing fact ⇒ [`ApplyError::HandlerData`].
/// 5. Build the new target content by appending the region byte slices
///    in `context.fact_ids` order, separated by a newline. Existing
///    target content is preserved verbatim above the appended block.
/// 6. Build the new source content by splicing out the moved spans.
/// 7. Atomically write target + source.
/// 8. Update `fact_index` for each moved row: `source_path` → target,
///    `region_start/end` → new offsets within the target page.
/// 9. Serialise [`PromoteSpec`] and return it as the `spec` JSON the
///    chassis stamps onto the proposal row.
///
/// The handler is idempotent on retry only in the trivial sense that
/// step 4 will fail (`HandlerData`) once the markers are no longer on
/// the source page — the caller can read the error and confirm the
/// move already happened.
///
/// # Errors
///
/// All failure modes funnel into [`ApplyError`]; see the variant docs
/// for which class each one maps to at the MCP boundary.
#[allow(
    clippy::too_many_lines,
    reason = "linear apply pipeline; splitting hides the order"
)]
async fn apply_paragraph_to_file(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: PromoteContext = parse_context(context)?;
    let ans: PromoteAnswers = parse_answers(answers)?;

    let source_page_path = validated_page_path(&ctx.source_page, "context.source_page")?;
    let target_page_path = validated_page_path(&ans.target_page, "answers.target_page")?;
    if source_page_path == target_page_path {
        return Err(ApplyError::InvalidPayload(
            "answers.target_page must differ from context.source_page".into(),
        ));
    }
    let wiki_id = WikiId::parse(&ctx.source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    let fact_ids = parse_fact_ids(&ctx.fact_ids)?;

    let handle = tree
        .locate(&wiki_id)
        .map_err(|e| ApplyError::HandlerData(format!("wiki not found: {e}")))?;

    let source_abs = handle.abs_dir().join(&source_page_path);
    let target_abs = handle.abs_dir().join(&target_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let target_rel = wiki::workdir_relative_source_path(tree.workdir(), &target_abs);

    // Validate every fact is active and currently lives in source_page.
    for fid in &fact_ids {
        let row = fact_index::find_by_id(pool, fid)
            .await
            .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
            .ok_or_else(|| ApplyError::HandlerData(format!("fact {fid} not in fact_index")))?;
        if row.superseded_at.is_some() || row.deleted_at.is_some() {
            return Err(ApplyError::HandlerData(format!(
                "fact {fid} is superseded or tombstoned",
            )));
        }
        if row.source_path != source_rel {
            return Err(ApplyError::HandlerData(format!(
                "fact {fid} lives at {actual}, not at expected source {expected}",
                actual = row.source_path,
                expected = source_rel,
            )));
        }
    }

    // Read source page contents.
    let source_contents = std::fs::read_to_string(&source_abs)
        .map_err(|e| ApplyError::HandlerIo(format!("read {source_rel}: {e}")))?;

    // Parse and collect the regions we're going to move.
    let parsed = parser::parse(&source_contents);
    let mut by_fact: HashMap<FactId, ParsedRegion> = HashMap::new();
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fid) = attrs.fact_id
        {
            by_fact.insert(
                fid,
                ParsedRegion {
                    start,
                    end,
                    bytes: source_contents[start..end].to_owned(),
                },
            );
        }
    }
    let mut moved: Vec<MovedRegion> = Vec::with_capacity(fact_ids.len());
    for fid in &fact_ids {
        let region = by_fact.remove(fid).ok_or_else(|| {
            ApplyError::HandlerData(format!(
                "fact {fid} not present as a marker in {source_rel}",
            ))
        })?;
        moved.push(MovedRegion {
            fact_id: fid.clone(),
            old_start: region.start,
            old_end: region.end,
            bytes: region.bytes,
        });
    }

    // Read existing target content (may be empty / not exist).
    let target_existed_before = target_abs.exists();
    let existing_target = if target_existed_before {
        std::fs::read_to_string(&target_abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {target_rel}: {e}")))?
    } else {
        String::new()
    };

    // Compose new target = existing target + appended regions, recording
    // new byte offsets per fact_id.
    let (new_target, target_offsets) = compose_target(&existing_target, &moved);

    // Compose new source = source minus the moved spans.
    let new_source = compose_source_minus_moved(&source_contents, &moved);

    // DB rows FIRST, files second (the capture commit-point pattern):
    // repoint every row at the target with NULL offsets — a "pending
    // render" the orphan sweep spares on both pages — so at no instant
    // does the DB claim a fact lives on a page whose disk bytes no
    // longer carry its marker. A watcher reindex racing the two writes
    // below can then never mistake the in-flight move for a hand
    // deletion of the markers.
    let mut repointed: Vec<&MovedRegion> = Vec::with_capacity(moved.len());
    let mut failure: Option<ApplyError> = None;
    for m in &moved {
        match fact_index::move_region(pool, &m.fact_id, &target_rel, None, None).await {
            Ok(0) => {
                failure = Some(ApplyError::HandlerData(format!(
                    "fact_index::move_region updated 0 rows for {fid}",
                    fid = m.fact_id,
                )));
                break;
            },
            Ok(_) => repointed.push(m),
            Err(e) => {
                failure = Some(ApplyError::HandlerIo(e.to_string()));
                break;
            },
        }
    }

    // Atomic writes (target first so the markers are reachable on disk
    // for the brief moment between writes; if step 2 fails, a retry will
    // find the regions are already on both pages — the apply must then
    // be cleared manually because we cannot atomically write two files).
    if failure.is_none() {
        failure = atomic_write(&target_abs, new_target.as_bytes())
            .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {target_rel}: {e}")))
            .err()
            .or_else(|| {
                atomic_write(&source_abs, new_source.as_bytes())
                    .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {source_rel}: {e}")))
                    .err()
            });
    }

    if let Some(err) = failure {
        // Compensate: point the already-repointed rows back at the
        // source page with their original offsets. Best-effort — a row
        // left behind stays a pending render on the target page, which
        // the next compile re-emits; residue, never a loss.
        for m in repointed {
            if let Err(e) = fact_index::move_region(
                pool,
                &m.fact_id,
                &source_rel,
                Some(i64::try_from(m.old_start).unwrap_or(i64::MAX)),
                Some(i64::try_from(m.old_end).unwrap_or(i64::MAX)),
            )
            .await
            {
                tracing::error!(
                    fact_id = m.fact_id.as_str(),
                    error = %e,
                    "promote: apply failed AND rollback repoint failed — row left as pending render on target"
                );
            }
        }
        return Err(err);
    }

    // Stamp the rendered offsets now that the markers are on disk.
    let mut moved_records = Vec::with_capacity(moved.len());
    for m in &moved {
        let off = target_offsets.get(&m.fact_id).copied().ok_or_else(|| {
            ApplyError::HandlerData(format!(
                "internal: target offsets missing for {fid}",
                fid = m.fact_id
            ))
        })?;
        let touched = fact_index::move_region(
            pool,
            &m.fact_id,
            &target_rel,
            Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
            Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
        )
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(ApplyError::HandlerData(format!(
                "fact_index::move_region updated 0 rows for {fid} at offset stamp",
                fid = m.fact_id,
            )));
        }
        moved_records.push(MovedFactRecord {
            fact_id: m.fact_id.as_str().to_owned(),
            old_region_start: i64::try_from(m.old_start).unwrap_or(i64::MAX),
            old_region_end: i64::try_from(m.old_end).unwrap_or(i64::MAX),
            new_region_start: i64::try_from(off.0).unwrap_or(i64::MAX),
            new_region_end: i64::try_from(off.1).unwrap_or(i64::MAX),
        });
    }

    tracing::info!(
        wiki_id = ctx.source_wiki_id.as_str(),
        source = source_rel,
        target = target_rel,
        moved = moved_records.len(),
        "promote: paragraph_to_file applied",
    );

    let spec = PromoteSpec {
        variant: VARIANT_PARAGRAPH_TO_FILE.to_owned(),
        source_wiki_id: ctx.source_wiki_id,
        source_page: source_page_path.to_string_lossy().into_owned(),
        target_page: target_page_path.to_string_lossy().into_owned(),
        target_existed_before,
        moved_facts: moved_records,
    };
    Ok(json!(spec))
}

// ---------- Revert ----------

/// Revert a previously-applied `wiki_promote` proposal.
///
/// Reads the `spec` JSON the chassis stored at apply time and undoes
/// the move: each fact's marker is taken back off the target page,
/// re-appended at the end of the source page, and its `fact_index` row
/// is repointed at the source. The user's manual edits to either page
/// between apply and revert are preserved (the parser looks the regions
/// up by `fact_id`, not by byte offset).
///
/// If the target page only existed because of the apply (no prior
/// content) and revert empties it back to zero regions of *our* facts,
/// the file is left in place — it will simply contain the user's
/// non-marker prose, if any. The caller can `wiki_forget` the page
/// separately if they want it gone.
///
/// # Errors
///
/// All failure modes funnel into [`RevertError`].
#[allow(
    clippy::too_many_lines,
    reason = "linear revert pipeline; splitting hides the DB-first order"
)]
async fn revert_paragraph_to_file(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let spec: PromoteSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a PromoteSpec: {e}")))?;
    if spec.variant != VARIANT_PARAGRAPH_TO_FILE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {actual} is not paragraph_to_file",
            actual = spec.variant,
        )));
    }
    let source_page_path = validated_page_path_rev(&spec.source_page, "spec.source_page")?;
    let target_page_path = validated_page_path_rev(&spec.target_page, "spec.target_page")?;
    let wiki_id = WikiId::parse(&spec.source_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.source_wiki_id invalid: {e}")))?;

    let handle = tree
        .locate(&wiki_id)
        .map_err(|e| RevertError::HandlerData(format!("wiki not found: {e}")))?;
    let source_abs = handle.abs_dir().join(&source_page_path);
    let target_abs = handle.abs_dir().join(&target_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let target_rel = wiki::workdir_relative_source_path(tree.workdir(), &target_abs);

    let target_contents = std::fs::read_to_string(&target_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {target_rel}: {e}")))?;

    // Locate each fact's region on the target by fact_id (not by stored
    // byte offsets — the user may have edited the target since apply).
    let parsed = parser::parse(&target_contents);
    let mut by_fact: HashMap<FactId, ParsedRegion> = HashMap::new();
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fid) = attrs.fact_id
        {
            by_fact.insert(
                fid,
                ParsedRegion {
                    start,
                    end,
                    bytes: target_contents[start..end].to_owned(),
                },
            );
        }
    }

    let mut moved_back: Vec<MovedRegion> = Vec::with_capacity(spec.moved_facts.len());
    for rec in &spec.moved_facts {
        let fid = FactId::parse(&rec.fact_id).map_err(|e| {
            RevertError::InvalidPayload(format!("spec fact_id {} invalid: {e}", rec.fact_id))
        })?;
        let region = by_fact.remove(&fid).ok_or_else(|| {
            RevertError::HandlerData(format!(
                "fact {fid} not present on target {target_rel} — manual edit lost the marker",
            ))
        })?;
        moved_back.push(MovedRegion {
            fact_id: fid,
            old_start: region.start,
            old_end: region.end,
            bytes: region.bytes,
        });
    }

    // New target = current target minus the moved-back regions.
    let new_target = compose_source_minus_moved(&target_contents, &moved_back);

    // New source = current source + appended regions.
    let source_contents = std::fs::read_to_string(&source_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {source_rel}: {e}")))?;
    let (new_source, source_offsets) = compose_target(&source_contents, &moved_back);

    // DB rows FIRST, files second — same race shield as the apply: a
    // row repointed at the source with NULL offsets is a pending render
    // the orphan sweep spares, so a watcher reindex of the target page
    // mid-revert cannot tombstone the fact when its marker leaves that
    // page.
    for m in &moved_back {
        let touched = fact_index::move_region(pool, &m.fact_id, &source_rel, None, None)
            .await
            .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(RevertError::HandlerData(format!(
                "fact_index::move_region updated 0 rows for {fid}",
                fid = m.fact_id,
            )));
        }
    }

    atomic_write(&source_abs, new_source.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {source_rel}: {e}")))?;
    atomic_write(&target_abs, new_target.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {target_rel}: {e}")))?;

    // Stamp the rendered offsets on the source page.
    for m in &moved_back {
        let off = source_offsets.get(&m.fact_id).copied().ok_or_else(|| {
            RevertError::HandlerData(format!(
                "internal: source offsets missing for {fid}",
                fid = m.fact_id,
            ))
        })?;
        let touched = fact_index::move_region(
            pool,
            &m.fact_id,
            &source_rel,
            Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
            Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
        )
        .await
        .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(RevertError::HandlerData(format!(
                "fact_index::move_region updated 0 rows for {fid} at offset stamp",
                fid = m.fact_id,
            )));
        }
    }

    tracing::info!(
        wiki_id = spec.source_wiki_id.as_str(),
        source = source_rel,
        target = target_rel,
        moved_back = moved_back.len(),
        "promote: paragraph_to_file reverted",
    );

    // Plan-sync seam (inverse): re-home the reverted facts back onto the
    // source page's plan slug — otherwise the persisted plan keeps them on
    // the target and the next recompile re-applies the move the operator
    // just reverted. Best-effort: the disk/DB revert stands either way.
    rehome_after_move(
        pool,
        &moved_back,
        &plan_slug_of_page(&spec.source_wiki_id, &spec.source_page),
        &spec.source_wiki_id,
        &[],
        tree,
    )
    .await;

    Ok(())
}

// ---------- fact refile variant (cross-wiki single-fact move) ----------

/// Apply a `fact_refile`: move **one** fact from a page of the source
/// wiki to a page of a **different** (existing) destination wiki — the
/// REM cross-wiki refile sub-job's act-first verb.
///
/// This is the paragraph-to-file pipeline lifted across the wiki
/// boundary for a single fact: locate **both** wikis, splice the one
/// region off the source page, weave it onto the destination page, and
/// repoint the `fact_index` row's `wiki_id` (via [`fact_index::move_to_wiki`],
/// the only primitive that touches `wiki_id` — `move_region` never does).
///
/// The destination page path is **wiki-relative** (joined onto the dest
/// wiki's `abs_dir`); a workdir-relative path would double the
/// `wikis/<id>/` prefix and miss on disk.
///
/// The commit order is load-bearing, exactly as in `apply_paragraph_to_file`:
/// the DB row is repointed FIRST with NULL offsets (a pending render the
/// orphan sweep spares on both pages), THEN the destination + source
/// files are written, THEN the rendered offsets are stamped. A watcher
/// reindex racing the writes can then never mistake the in-flight move
/// for a hand deletion of the marker.
///
/// # Errors
///
/// All failure modes funnel into [`ApplyError`].
#[allow(
    clippy::too_many_lines,
    reason = "linear apply pipeline; splitting hides the DB-first order"
)]
async fn apply_fact_refile(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    answers: &Value,
) -> Result<Value, ApplyError> {
    let source_wiki_id = json_str(context, "source_wiki_id", "context.source_wiki_id")?;
    let source_page = json_str(context, "source_page", "context.source_page")?;
    let dest_wiki_id = json_str(answers, "dest_wiki_id", "answers.dest_wiki_id")?;
    let dest_page = json_str(answers, "dest_page", "answers.dest_page")?;
    let fact_id_str = json_str(context, "fact_id", "context.fact_id")?;

    if source_wiki_id == dest_wiki_id {
        return Err(ApplyError::InvalidPayload(
            "fact_refile is cross-wiki: dest_wiki_id must differ from source_wiki_id".into(),
        ));
    }
    let source_page_path = validated_page_path(&source_page, "context.source_page")?;
    let dest_page_path = validated_page_path(&dest_page, "answers.dest_page")?;
    let source_wiki = WikiId::parse(&source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    let dest_wiki = WikiId::parse(&dest_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("answers.dest_wiki_id invalid: {e}")))?;
    let fact_id = FactId::parse(&fact_id_str)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.fact_id invalid: {e}")))?;

    let source_handle = tree
        .locate(&source_wiki)
        .map_err(|e| ApplyError::HandlerData(format!("source wiki not found: {e}")))?;
    let dest_handle = tree
        .locate(&dest_wiki)
        .map_err(|e| ApplyError::HandlerData(format!("dest wiki not found: {e}")))?;

    let source_abs = source_handle.abs_dir().join(&source_page_path);
    let dest_abs = dest_handle.abs_dir().join(&dest_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let dest_rel = wiki::workdir_relative_source_path(tree.workdir(), &dest_abs);

    // The fact must be active and currently live on the source page.
    let row = fact_index::find_by_id(pool, &fact_id)
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
        .ok_or_else(|| ApplyError::HandlerData(format!("fact {fact_id} not in fact_index")))?;
    if row.superseded_at.is_some() || row.deleted_at.is_some() {
        return Err(ApplyError::HandlerData(format!(
            "fact {fact_id} is superseded or tombstoned",
        )));
    }
    if row.source_path != source_rel {
        return Err(ApplyError::HandlerData(format!(
            "fact {fact_id} lives at {actual}, not at expected source {source_rel}",
            actual = row.source_path,
        )));
    }
    if row.wiki_id != source_wiki_id {
        return Err(ApplyError::HandlerData(format!(
            "fact {fact_id} belongs to wiki {actual}, not {source_wiki_id}",
            actual = row.wiki_id,
        )));
    }

    // Read the source page and locate the one region by fact_id.
    let source_contents = std::fs::read_to_string(&source_abs)
        .map_err(|e| ApplyError::HandlerIo(format!("read {source_rel}: {e}")))?;
    let parsed = parser::parse(&source_contents);
    let mut region: Option<ParsedRegion> = None;
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && attrs.fact_id.as_ref() == Some(&fact_id)
        {
            region = Some(ParsedRegion {
                start,
                end,
                bytes: source_contents[start..end].to_owned(),
            });
            break;
        }
    }
    let region = region.ok_or_else(|| {
        ApplyError::HandlerData(format!(
            "fact {fact_id} not present as a marker in {source_rel}",
        ))
    })?;
    let moved = vec![MovedRegion {
        fact_id: fact_id.clone(),
        old_start: region.start,
        old_end: region.end,
        bytes: region.bytes,
    }];

    // Read existing destination content (may be empty / not exist).
    let target_existed_before = dest_abs.exists();
    let existing_target = if target_existed_before {
        std::fs::read_to_string(&dest_abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {dest_rel}: {e}")))?
    } else {
        String::new()
    };
    let (new_target, target_offsets) = compose_target(&existing_target, &moved);
    let new_source = compose_source_minus_moved(&source_contents, &moved);

    // DB row FIRST: repoint wiki_id + source_path with NULL offsets — a
    // pending render the orphan sweep spares on both pages.
    match fact_index::move_to_wiki(pool, &fact_id, dest_wiki.as_str(), &dest_rel, None, None).await
    {
        Ok(0) => {
            return Err(ApplyError::HandlerData(format!(
                "fact_index::move_to_wiki updated 0 rows for {fact_id}",
            )));
        },
        Ok(_) => {},
        Err(e) => return Err(ApplyError::HandlerIo(e.to_string())),
    }

    // Atomic writes (dest first so the marker is reachable on disk for
    // the brief moment between writes), with compensation on failure:
    // repoint the row back at the source page + wiki with its original
    // offsets so nothing is stranded.
    let write_err = atomic_write(&dest_abs, new_target.as_bytes())
        .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {dest_rel}: {e}")))
        .err()
        .or_else(|| {
            atomic_write(&source_abs, new_source.as_bytes())
                .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {source_rel}: {e}")))
                .err()
        });
    if let Some(err) = write_err {
        if let Err(e) = fact_index::move_to_wiki(
            pool,
            &fact_id,
            source_wiki.as_str(),
            &source_rel,
            Some(i64::try_from(moved[0].old_start).unwrap_or(i64::MAX)),
            Some(i64::try_from(moved[0].old_end).unwrap_or(i64::MAX)),
        )
        .await
        {
            tracing::error!(
                fact_id = fact_id.as_str(),
                error = %e,
                "refile: apply failed AND rollback repoint failed — row left as pending render on dest"
            );
        }
        return Err(err);
    }

    // Stamp the rendered offsets now that the marker is on disk.
    let off = target_offsets.get(&fact_id).copied().ok_or_else(|| {
        ApplyError::HandlerData(format!("internal: dest offsets missing for {fact_id}"))
    })?;
    let touched = fact_index::move_to_wiki(
        pool,
        &fact_id,
        dest_wiki.as_str(),
        &dest_rel,
        Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
        Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
    )
    .await
    .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
    if touched == 0 {
        return Err(ApplyError::HandlerData(format!(
            "fact_index::move_to_wiki updated 0 rows for {fact_id} at offset stamp",
        )));
    }

    // Plan-sync seam: re-home the fact onto the DEST page in the persisted
    // plan. `RehomePageSeed` natively carries `wiki_id`, so the cross-wiki
    // rehome is native — the seam detaches the fact from the source page
    // (force-dirtying it so the source recompiles WITHOUT the fact) and
    // attaches it to the dest page (force-dirtying it so the dest weaves
    // the fact in). Best-effort: the disk/DB move stands regardless.
    let dest_seed = crate::planner::RehomePageSeed::page_in_wiki(&dest_page, &dest_wiki_id);
    rehome_rows_with_seed(pool, std::slice::from_ref(&fact_id), &dest_seed, &[], tree).await;

    tracing::info!(
        source_wiki = source_wiki_id.as_str(),
        source = source_rel,
        dest_wiki = dest_wiki_id.as_str(),
        dest = dest_rel,
        fact_id = fact_id.as_str(),
        "promote: fact_refile applied",
    );

    let spec = FactRefileSpec {
        variant: VARIANT_FACT_REFILE.to_owned(),
        source_wiki_id,
        source_page: source_page_path.to_string_lossy().into_owned(),
        dest_wiki_id,
        dest_page: dest_page_path.to_string_lossy().into_owned(),
        target_existed_before,
        moved: MovedFactRecord {
            fact_id: fact_id.as_str().to_owned(),
            old_region_start: i64::try_from(moved[0].old_start).unwrap_or(i64::MAX),
            old_region_end: i64::try_from(moved[0].old_end).unwrap_or(i64::MAX),
            new_region_start: i64::try_from(off.0).unwrap_or(i64::MAX),
            new_region_end: i64::try_from(off.1).unwrap_or(i64::MAX),
        },
    };
    Ok(json!(spec))
}

/// Revert a previously-applied `fact_refile`: take the marker back off
/// the destination page, re-append it on the source page, and repoint
/// the `fact_index` row's `wiki_id` back to the source wiki. The user's
/// manual edits to either page are preserved (the parser looks the region
/// up by `fact_id`, not by byte offset).
///
/// # Errors
///
/// All failure modes funnel into [`RevertError`].
#[allow(
    clippy::too_many_lines,
    reason = "linear revert pipeline; splitting hides the DB-first order"
)]
async fn revert_fact_refile(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let spec: FactRefileSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a FactRefileSpec: {e}")))?;
    if spec.variant != VARIANT_FACT_REFILE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {actual} is not {VARIANT_FACT_REFILE}",
            actual = spec.variant,
        )));
    }
    let source_page_path = validated_page_path_rev(&spec.source_page, "spec.source_page")?;
    let dest_page_path = validated_page_path_rev(&spec.dest_page, "spec.dest_page")?;
    let source_wiki = WikiId::parse(&spec.source_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.source_wiki_id invalid: {e}")))?;
    let dest_wiki = WikiId::parse(&spec.dest_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.dest_wiki_id invalid: {e}")))?;
    let fact_id = FactId::parse(&spec.moved.fact_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.moved.fact_id invalid: {e}")))?;

    let source_handle = tree
        .locate(&source_wiki)
        .map_err(|e| RevertError::HandlerData(format!("source wiki not found: {e}")))?;
    let dest_handle = tree
        .locate(&dest_wiki)
        .map_err(|e| RevertError::HandlerData(format!("dest wiki not found: {e}")))?;
    let source_abs = source_handle.abs_dir().join(&source_page_path);
    let dest_abs = dest_handle.abs_dir().join(&dest_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let dest_rel = wiki::workdir_relative_source_path(tree.workdir(), &dest_abs);

    // Locate the fact's region on the dest by fact_id.
    let dest_contents = std::fs::read_to_string(&dest_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {dest_rel}: {e}")))?;
    let parsed = parser::parse(&dest_contents);
    let mut region: Option<ParsedRegion> = None;
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && attrs.fact_id.as_ref() == Some(&fact_id)
        {
            region = Some(ParsedRegion {
                start,
                end,
                bytes: dest_contents[start..end].to_owned(),
            });
            break;
        }
    }
    let region = region.ok_or_else(|| {
        RevertError::HandlerData(format!(
            "fact {fact_id} not present on dest {dest_rel} — manual edit lost the marker",
        ))
    })?;
    let moved_back = vec![MovedRegion {
        fact_id: fact_id.clone(),
        old_start: region.start,
        old_end: region.end,
        bytes: region.bytes,
    }];

    let new_dest = compose_source_minus_moved(&dest_contents, &moved_back);
    let source_contents = std::fs::read_to_string(&source_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {source_rel}: {e}")))?;
    let (new_source, source_offsets) = compose_target(&source_contents, &moved_back);

    // DB row FIRST: repoint wiki_id + source_path back to the source with
    // NULL offsets — the same race shield as the apply.
    let touched = fact_index::move_to_wiki(
        pool,
        &fact_id,
        source_wiki.as_str(),
        &source_rel,
        None,
        None,
    )
    .await
    .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
    if touched == 0 {
        return Err(RevertError::HandlerData(format!(
            "fact_index::move_to_wiki updated 0 rows for {fact_id}",
        )));
    }

    atomic_write(&source_abs, new_source.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {source_rel}: {e}")))?;
    atomic_write(&dest_abs, new_dest.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {dest_rel}: {e}")))?;

    let off = source_offsets.get(&fact_id).copied().ok_or_else(|| {
        RevertError::HandlerData(format!("internal: source offsets missing for {fact_id}"))
    })?;
    let touched = fact_index::move_to_wiki(
        pool,
        &fact_id,
        source_wiki.as_str(),
        &source_rel,
        Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
        Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
    )
    .await
    .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
    if touched == 0 {
        return Err(RevertError::HandlerData(format!(
            "fact_index::move_to_wiki updated 0 rows for {fact_id} at offset stamp",
        )));
    }

    tracing::info!(
        source_wiki = spec.source_wiki_id.as_str(),
        source = source_rel,
        dest_wiki = spec.dest_wiki_id.as_str(),
        dest = dest_rel,
        fact_id = fact_id.as_str(),
        "promote: fact_refile reverted",
    );

    // Plan-sync seam (inverse): re-home the fact back onto the source page
    // (in the source wiki) so the persisted plan recompiles both pages.
    let source_seed =
        crate::planner::RehomePageSeed::page_in_wiki(&spec.source_page, &spec.source_wiki_id);
    rehome_rows_with_seed(
        pool,
        std::slice::from_ref(&fact_id),
        &source_seed,
        &[],
        tree,
    )
    .await;

    Ok(())
}

// ---------- page merge variant ----------

/// Plan slug of a wiki-relative concept page path (`viaggi.md` → `viaggi`,
/// nested paths flatten like the ingest placement). `index.md` maps to the
/// wiki's own foundation slug.
fn plan_slug_of_page(wiki_id: &str, page: &str) -> String {
    let stem = page.strip_suffix(".md").unwrap_or(page);
    if stem == "index" {
        crate::planner::slugify(wiki_id)
    } else {
        crate::planner::slugify(stem)
    }
}

/// Best-effort plan-sync after a move/revert: re-home `moved` facts onto
/// `dest_slug` in the persisted plan (seeding from `seed_wiki`), removing
/// `remove_pages` husks. Failures are logged loudly, never returned — the
/// disk/DB change already stands and the seam is repairable by hand or by
/// the next full rebuild of the plan.
async fn rehome_after_move(
    pool: &SqlitePool,
    moved: &[MovedRegion],
    dest_slug: &str,
    seed_wiki: &str,
    remove_pages: &[String],
    tree: &WikiTree,
) {
    let ids: Vec<FactId> = moved.iter().map(|m| m.fact_id.clone()).collect();
    let seed = crate::planner::RehomePageSeed::concept(dest_slug, seed_wiki);
    rehome_rows_with_seed(pool, &ids, &seed, remove_pages, tree).await;
}

/// Best-effort plan-sync with an explicit destination seed — the shared
/// core of [`rehome_after_move`] and the emergence (`file_to_subwiki`)
/// seam, where the destination is the emerged wiki's `index.md` rather
/// than a `<slug>.md` concept leaf. Failures are logged loudly, never
/// returned.
async fn rehome_rows_with_seed(
    pool: &SqlitePool,
    fact_ids: &[FactId],
    seed: &crate::planner::RehomePageSeed,
    remove_pages: &[String],
    tree: &WikiTree,
) {
    let mut rows = Vec::with_capacity(fact_ids.len());
    for fid in fact_ids {
        match fact_index::find_by_id(pool, fid).await {
            Ok(Some(r)) => rows.push(r),
            Ok(None) => {},
            Err(e) => {
                tracing::error!(fact_id = fid.as_str(), error = %e, "promote: plan re-home row load failed");
            },
        }
    }
    let plan_moves: Vec<(&fact_index::FactIndexRow, &crate::planner::RehomePageSeed)> =
        rows.iter().map(|r| (r, seed)).collect();
    if let Err(e) = crate::planner::rehome_facts_in_persisted_plan(
        tree,
        &plan_moves,
        remove_pages,
        &chrono::Utc::now().to_rfc3339(),
    ) {
        tracing::error!(
            dest = seed.slug,
            error = %e,
            "promote: plan re-home failed — the persisted plan is stale until the next full rebuild"
        );
    }
}

/// Context fields for the page-merge variant: the husk's facts plus the
/// identity of both pages (presentation + the revert's plan re-seed).
#[derive(Debug, Clone, Deserialize)]
struct MergeContext {
    source_wiki_id: String,
    /// The survivor's wiki. `None` (receipts predating family-scope
    /// merges) = same as `source_wiki_id`.
    #[serde(default)]
    target_wiki_id: Option<String>,
    /// The husk page (wiki-relative `.md`) whose facts all move out.
    source_page: String,
    fact_ids: Vec<String>,
    #[serde(default)]
    husk_title: Option<String>,
    #[serde(default)]
    husk_description: Option<String>,
    #[serde(default)]
    husk_style: Option<String>,
}

/// `spec` payload of a successful page merge. Read back by
/// [`revert_page_merge`]: `husk_shell` (the husk contents minus the moved
/// regions) is what lets the revert recreate the deleted file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeSpec {
    variant: String,
    source_wiki_id: String,
    /// The survivor's wiki. `None` (pre-family-scope receipts) = same
    /// as `source_wiki_id`.
    #[serde(default)]
    target_wiki_id: Option<String>,
    /// The husk page path (deleted by the apply).
    source_page: String,
    /// The survivor page path.
    target_page: String,
    /// Husk contents minus the moved regions — the revert's skeleton.
    husk_shell: String,
    /// Husk identity for the revert's plan re-seed.
    husk_title: String,
    husk_description: String,
    husk_style: Option<String>,
    moved_facts: Vec<MovedFactRecord>,
}

/// Apply a `page_merge`: move **every** active fact of the husk page onto
/// the survivor, delete the husk file, and re-home the move in the
/// persisted compilation plan (husk dropped from plan + registry).
///
/// Refuses `index.md` on either side (foundation pages never merge), and
/// refuses a partial move: every active `fact_index` row living on the husk
/// must be in `context.fact_ids`, else deleting the file would strand rows
/// for the orphan sweep to tombstone.
#[allow(
    clippy::too_many_lines,
    reason = "linear apply pipeline; splitting hides the DB-first order"
)]
async fn apply_page_merge(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: MergeContext = serde_json::from_value(context.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))?;
    let ans: PromoteAnswers = parse_answers(answers)?;

    let source_page_path = validated_page_path(&ctx.source_page, "context.source_page")?;
    let target_page_path = validated_page_path(&ans.target_page, "answers.target_page")?;
    if source_page_path.as_os_str() == "index.md" || target_page_path.as_os_str() == "index.md" {
        return Err(ApplyError::InvalidPayload(
            "page_merge never touches a foundation index.md".into(),
        ));
    }
    let wiki_id = WikiId::parse(&ctx.source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    // The survivor's wiki: same as the husk's unless a family-scope merge
    // crossed the parent↔sub-wiki line (pre-family receipts carry None).
    let target_wiki_str = ctx
        .target_wiki_id
        .clone()
        .unwrap_or_else(|| ctx.source_wiki_id.clone());
    let target_wiki = WikiId::parse(&target_wiki_str)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.target_wiki_id invalid: {e}")))?;
    let cross_wiki = target_wiki_str != ctx.source_wiki_id;
    let fact_ids = parse_fact_ids(&ctx.fact_ids)?;

    let handle = tree
        .locate(&wiki_id)
        .map_err(|e| ApplyError::HandlerData(format!("wiki not found: {e}")))?;
    let target_handle = tree
        .locate(&target_wiki)
        .map_err(|e| ApplyError::HandlerData(format!("survivor wiki not found: {e}")))?;
    let source_abs = handle.abs_dir().join(&source_page_path);
    let target_abs = target_handle.abs_dir().join(&target_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let target_rel = wiki::workdir_relative_source_path(tree.workdir(), &target_abs);
    if source_rel == target_rel {
        return Err(ApplyError::InvalidPayload(
            "answers.target_page must differ from context.source_page".into(),
        ));
    }

    // Validate every fact is active and lives on the husk — and that the
    // set is COMPLETE: an active row on the husk that is not part of the
    // move would be stranded when the file is deleted.
    for fid in &fact_ids {
        let row = fact_index::find_by_id(pool, fid)
            .await
            .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
            .ok_or_else(|| ApplyError::HandlerData(format!("fact {fid} not in fact_index")))?;
        if row.superseded_at.is_some() || row.deleted_at.is_some() {
            return Err(ApplyError::HandlerData(format!(
                "fact {fid} is superseded or tombstoned",
            )));
        }
        if row.source_path != source_rel {
            return Err(ApplyError::HandlerData(format!(
                "fact {fid} lives at {actual}, not at the husk {source_rel}",
                actual = row.source_path,
            )));
        }
    }
    let moving: HashSet<&str> = fact_ids.iter().map(FactId::as_str).collect();
    for row in fact_index::find_active_in_wiki(pool, ctx.source_wiki_id.as_str())
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
    {
        if row.source_path == source_rel && !moving.contains(row.fact_id.as_str()) {
            return Err(ApplyError::HandlerData(format!(
                "page_merge must move every active fact of the husk; {fid} is not in the set",
                fid = row.fact_id,
            )));
        }
    }

    let source_contents = std::fs::read_to_string(&source_abs)
        .map_err(|e| ApplyError::HandlerIo(format!("read {source_rel}: {e}")))?;
    let parsed = parser::parse(&source_contents);
    let mut by_fact: HashMap<FactId, ParsedRegion> = HashMap::new();
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fid) = attrs.fact_id
        {
            by_fact.insert(
                fid,
                ParsedRegion {
                    start,
                    end,
                    bytes: source_contents[start..end].to_owned(),
                },
            );
        }
    }
    let mut moved: Vec<MovedRegion> = Vec::with_capacity(fact_ids.len());
    for fid in &fact_ids {
        let region = by_fact.remove(fid).ok_or_else(|| {
            ApplyError::HandlerData(format!(
                "fact {fid} not present as a marker in {source_rel}",
            ))
        })?;
        moved.push(MovedRegion {
            fact_id: fid.clone(),
            old_start: region.start,
            old_end: region.end,
            bytes: region.bytes,
        });
    }

    let existing_target = if target_abs.exists() {
        std::fs::read_to_string(&target_abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {target_rel}: {e}")))?
    } else {
        String::new()
    };
    let (new_target, target_offsets) = compose_target(&existing_target, &moved);
    // The husk minus its regions — stored in the spec so the revert can
    // recreate the deleted file (frontmatter + connective prose preserved).
    let husk_shell = compose_source_minus_moved(&source_contents, &moved);

    // DB rows FIRST (the capture commit-point pattern): repoint every row at
    // the survivor as a pending render so neither the husk deletion nor the
    // survivor write can be misread by the orphan sweep. A family-scope
    // merge that crossed the parent↔sub-wiki line re-homes the row's
    // `wiki_id` too (`move_to_wiki` — the only primitive that flips it).
    let mut repointed: Vec<&MovedRegion> = Vec::with_capacity(moved.len());
    let mut failure: Option<ApplyError> = None;
    for m in &moved {
        let res = if cross_wiki {
            fact_index::move_to_wiki(pool, &m.fact_id, &target_wiki_str, &target_rel, None, None)
                .await
        } else {
            fact_index::move_region(pool, &m.fact_id, &target_rel, None, None).await
        };
        match res {
            Ok(0) => {
                failure = Some(ApplyError::HandlerData(format!(
                    "fact_index repoint updated 0 rows for {fid}",
                    fid = m.fact_id,
                )));
                break;
            },
            Ok(_) => repointed.push(m),
            Err(e) => {
                failure = Some(ApplyError::HandlerIo(e.to_string()));
                break;
            },
        }
    }
    if failure.is_none() {
        failure = atomic_write(&target_abs, new_target.as_bytes())
            .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {target_rel}: {e}")))
            .err();
    }
    if let Some(err) = failure {
        // Compensate: point the repointed rows back at the husk (and back
        // into its wiki when the move crossed the line).
        for m in repointed {
            let back = if cross_wiki {
                fact_index::move_to_wiki(
                    pool,
                    &m.fact_id,
                    &ctx.source_wiki_id,
                    &source_rel,
                    Some(i64::try_from(m.old_start).unwrap_or(i64::MAX)),
                    Some(i64::try_from(m.old_end).unwrap_or(i64::MAX)),
                )
                .await
            } else {
                fact_index::move_region(
                    pool,
                    &m.fact_id,
                    &source_rel,
                    Some(i64::try_from(m.old_start).unwrap_or(i64::MAX)),
                    Some(i64::try_from(m.old_end).unwrap_or(i64::MAX)),
                )
                .await
            };
            if let Err(e) = back {
                tracing::error!(
                    fact_id = m.fact_id.as_str(),
                    error = %e,
                    "promote: merge apply failed AND rollback repoint failed — row left as pending render on survivor"
                );
            }
        }
        return Err(err);
    }

    // The survivor now carries every marker — the husk file can go. A
    // failed delete leaves only zombie markers (rows claim the survivor, so
    // the sweep spares them); warn and continue.
    if let Err(e) = std::fs::remove_file(&source_abs) {
        tracing::warn!(husk = source_rel, error = %e, "promote: husk delete failed — zombie markers remain");
    }

    // Stamp the rendered offsets on the survivor.
    let mut moved_records = Vec::with_capacity(moved.len());
    for m in &moved {
        let off = target_offsets.get(&m.fact_id).copied().ok_or_else(|| {
            ApplyError::HandlerData(format!(
                "internal: target offsets missing for {fid}",
                fid = m.fact_id
            ))
        })?;
        let touched = if cross_wiki {
            fact_index::move_to_wiki(
                pool,
                &m.fact_id,
                &target_wiki_str,
                &target_rel,
                Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
                Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
            )
            .await
        } else {
            fact_index::move_region(
                pool,
                &m.fact_id,
                &target_rel,
                Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
                Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
            )
            .await
        }
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(ApplyError::HandlerData(format!(
                "fact_index repoint updated 0 rows for {fid} at offset stamp",
                fid = m.fact_id,
            )));
        }
        moved_records.push(MovedFactRecord {
            fact_id: m.fact_id.as_str().to_owned(),
            old_region_start: i64::try_from(m.old_start).unwrap_or(i64::MAX),
            old_region_end: i64::try_from(m.old_end).unwrap_or(i64::MAX),
            new_region_start: i64::try_from(off.0).unwrap_or(i64::MAX),
            new_region_end: i64::try_from(off.1).unwrap_or(i64::MAX),
        });
    }

    // Plan-sync seam: the survivor gains the facts (seeded in ITS wiki —
    // family-scope merges may cross the line), the husk leaves the plan +
    // registry, both park on force_dirty (the survivor's raw appended
    // records get woven by the next compile).
    let husk_slug = plan_slug_of_page(&ctx.source_wiki_id, &ctx.source_page);
    rehome_after_move(
        pool,
        &moved,
        &plan_slug_of_page(&target_wiki_str, &ans.target_page),
        &target_wiki_str,
        std::slice::from_ref(&husk_slug),
        tree,
    )
    .await;

    tracing::info!(
        wiki_id = ctx.source_wiki_id.as_str(),
        survivor_wiki_id = target_wiki_str.as_str(),
        husk = source_rel,
        survivor = target_rel,
        moved = moved_records.len(),
        "promote: page_merge applied",
    );

    let spec = MergeSpec {
        variant: VARIANT_PAGE_MERGE.to_owned(),
        source_wiki_id: ctx.source_wiki_id,
        target_wiki_id: Some(target_wiki_str),
        source_page: source_page_path.to_string_lossy().into_owned(),
        target_page: target_page_path.to_string_lossy().into_owned(),
        husk_shell,
        husk_title: ctx.husk_title.unwrap_or_default(),
        husk_description: ctx.husk_description.unwrap_or_default(),
        husk_style: ctx.husk_style,
        moved_facts: moved_records,
    };
    Ok(json!(spec))
}

/// Revert a `page_merge`: take each fact's region (current bytes — user
/// edits on the survivor are preserved) back off the survivor, recreate the
/// husk file from the stored shell, and re-home the facts onto the husk's
/// plan slug (re-seeded from the stored identity).
#[allow(
    clippy::too_many_lines,
    reason = "linear revert pipeline; splitting hides the DB-first order"
)]
async fn revert_page_merge(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let spec: MergeSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a MergeSpec: {e}")))?;
    if spec.variant != VARIANT_PAGE_MERGE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {actual} is not page_merge",
            actual = spec.variant,
        )));
    }
    let source_page_path = validated_page_path_rev(&spec.source_page, "spec.source_page")?;
    let target_page_path = validated_page_path_rev(&spec.target_page, "spec.target_page")?;
    let wiki_id = WikiId::parse(&spec.source_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.source_wiki_id invalid: {e}")))?;
    // Pre-family-scope receipts carry no target wiki: same as the source.
    let target_wiki_str = spec
        .target_wiki_id
        .clone()
        .unwrap_or_else(|| spec.source_wiki_id.clone());
    let target_wiki = WikiId::parse(&target_wiki_str)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.target_wiki_id invalid: {e}")))?;
    let cross_wiki = target_wiki_str != spec.source_wiki_id;
    let handle = tree
        .locate(&wiki_id)
        .map_err(|e| RevertError::HandlerData(format!("wiki not found: {e}")))?;
    let target_handle = tree
        .locate(&target_wiki)
        .map_err(|e| RevertError::HandlerData(format!("survivor wiki not found: {e}")))?;
    let source_abs = handle.abs_dir().join(&source_page_path);
    let target_abs = target_handle.abs_dir().join(&target_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let target_rel = wiki::workdir_relative_source_path(tree.workdir(), &target_abs);

    let target_contents = std::fs::read_to_string(&target_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {target_rel}: {e}")))?;
    let parsed = parser::parse(&target_contents);
    let mut by_fact: HashMap<FactId, ParsedRegion> = HashMap::new();
    for ev in parsed.events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fid) = attrs.fact_id
        {
            by_fact.insert(
                fid,
                ParsedRegion {
                    start,
                    end,
                    bytes: target_contents[start..end].to_owned(),
                },
            );
        }
    }
    let mut moved_back: Vec<MovedRegion> = Vec::with_capacity(spec.moved_facts.len());
    for rec in &spec.moved_facts {
        let fid = FactId::parse(&rec.fact_id).map_err(|e| {
            RevertError::InvalidPayload(format!("spec fact_id {} invalid: {e}", rec.fact_id))
        })?;
        let region = by_fact.remove(&fid).ok_or_else(|| {
            RevertError::HandlerData(format!(
                "fact {fid} not present on survivor {target_rel} — manual edit lost the marker",
            ))
        })?;
        moved_back.push(MovedRegion {
            fact_id: fid,
            old_start: region.start,
            old_end: region.end,
            bytes: region.bytes,
        });
    }

    let new_target = compose_source_minus_moved(&target_contents, &moved_back);
    // Recreate the husk: the stored shell (frontmatter + connective prose)
    // plus the regions as they read NOW on the survivor.
    let (new_husk, husk_offsets) = compose_target(&spec.husk_shell, &moved_back);

    // DB rows FIRST — same race shield as the apply (and the same
    // wiki-flip when the merge crossed the family line).
    for m in &moved_back {
        let touched = if cross_wiki {
            fact_index::move_to_wiki(
                pool,
                &m.fact_id,
                &spec.source_wiki_id,
                &source_rel,
                None,
                None,
            )
            .await
        } else {
            fact_index::move_region(pool, &m.fact_id, &source_rel, None, None).await
        }
        .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(RevertError::HandlerData(format!(
                "fact_index repoint updated 0 rows for {fid}",
                fid = m.fact_id,
            )));
        }
    }
    atomic_write(&source_abs, new_husk.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {source_rel}: {e}")))?;
    atomic_write(&target_abs, new_target.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {target_rel}: {e}")))?;
    for m in &moved_back {
        let off = husk_offsets.get(&m.fact_id).copied().ok_or_else(|| {
            RevertError::HandlerData(format!(
                "internal: husk offsets missing for {fid}",
                fid = m.fact_id,
            ))
        })?;
        let touched = if cross_wiki {
            fact_index::move_to_wiki(
                pool,
                &m.fact_id,
                &spec.source_wiki_id,
                &source_rel,
                Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
                Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
            )
            .await
        } else {
            fact_index::move_region(
                pool,
                &m.fact_id,
                &source_rel,
                Some(i64::try_from(off.0).unwrap_or(i64::MAX)),
                Some(i64::try_from(off.1).unwrap_or(i64::MAX)),
            )
            .await
        }
        .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(RevertError::HandlerData(format!(
                "fact_index repoint updated 0 rows for {fid} at offset stamp",
                fid = m.fact_id,
            )));
        }
    }

    // Plan-sync seam (inverse): the husk page re-enters the plan + registry
    // with its stored identity and takes its facts back; both pages park on
    // force_dirty.
    let husk_slug = plan_slug_of_page(&spec.source_wiki_id, &spec.source_page);
    let seed = crate::planner::RehomePageSeed {
        slug: husk_slug,
        title: spec.husk_title.clone(),
        description: spec.husk_description.clone(),
        style: spec.husk_style.clone(),
        wiki_id: spec.source_wiki_id.clone(),
        page_path: None,
    };
    {
        let mut rows = Vec::with_capacity(moved_back.len());
        for m in &moved_back {
            if let Ok(Some(r)) = fact_index::find_by_id(pool, &m.fact_id).await {
                rows.push(r);
            }
        }
        let moves: Vec<(&fact_index::FactIndexRow, &crate::planner::RehomePageSeed)> =
            rows.iter().map(|r| (r, &seed)).collect();
        if let Err(e) = crate::planner::rehome_facts_in_persisted_plan(
            tree,
            &moves,
            &[],
            &chrono::Utc::now().to_rfc3339(),
        ) {
            tracing::error!(error = %e, "promote: merge revert plan re-home failed — plan stale until next rebuild");
        }
    }

    tracing::info!(
        wiki_id = spec.source_wiki_id.as_str(),
        husk = source_rel,
        survivor = target_rel,
        moved_back = moved_back.len(),
        "promote: page_merge reverted",
    );
    Ok(())
}

// ---------- file → sub-wiki variant ----------

/// Answers shape for the file → sub-wiki variant.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct SubwikiAnswers {
    /// Slug for the new sub-wiki. Defaults to the source page's
    /// filename stem (e.g. `giardinaggio.md` → `giardinaggio`). Goes
    /// through [`crate::slug::derive_slug`] before being used.
    #[serde(default)]
    new_wiki_slug: Option<String>,
    /// Human-readable title for the new sub-wiki. Defaults to the slug
    /// (after derivation).
    #[serde(default)]
    new_wiki_title: Option<String>,
}

/// `spec` payload for a successful file → sub-wiki apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubwikiSpec {
    variant: String,
    /// Original parent wiki id (where the source page used to live).
    source_wiki_id: String,
    /// Original page path within the parent wiki (e.g. `giardinaggio.md`).
    source_page: String,
    /// Verbatim byte content of the source page before the apply. The
    /// revert path writes this back to disk. Captured as a single
    /// string because `index.md` of the new sub-wiki is a verbatim
    /// copy of these bytes — preserving them in `spec` lets the revert
    /// be a clean `atomic_write` rather than a parse + reassemble.
    source_page_bytes: String,
    /// New sub-wiki id (parent + slug joined with `-` via
    /// [`WikiId::child_of`]).
    new_wiki_id: String,
    /// Slug used to build the new directory under the parent.
    new_wiki_slug: String,
    /// Fact ids moved. Their region offsets in the new `index.md` are
    /// identical to the offsets they had in the source page because
    /// the bytes are copied verbatim, so we do not need to store them.
    fact_ids: Vec<String>,
}

#[allow(
    clippy::too_many_lines,
    reason = "linear apply pipeline; splitting hides the order"
)]
async fn apply_file_to_subwiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: PromoteContext = parse_context(context)?;
    let ans: SubwikiAnswers = serde_json::from_value(answers.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("answers: {e}")))?;

    let source_page_path = validated_page_path(&ctx.source_page, "context.source_page")?;
    let parent_wiki_id = WikiId::parse(&ctx.source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    let fact_ids = parse_fact_ids(&ctx.fact_ids)?;

    // Derive the new sub-wiki slug.
    let slug_seed = ans.new_wiki_slug.as_deref().unwrap_or_else(|| {
        source_page_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
    });
    if slug_seed.is_empty() {
        return Err(ApplyError::InvalidPayload(
            "could not derive a slug for the new sub-wiki (source page has no stem and answers.new_wiki_slug is empty)".into(),
        ));
    }
    let derived = crate::slug::derive_slug(slug_seed)
        .map_err(|e| ApplyError::InvalidPayload(format!("new_wiki_slug derive: {e}")))?;
    let new_slug = WikiSlug::parse(&derived)
        .map_err(|e| ApplyError::InvalidPayload(format!("new_wiki_slug invalid: {e}")))?;
    let new_wiki_id = WikiId::child_of(&parent_wiki_id, &new_slug);

    // Locate the parent + verify the new sub-wiki directory does not exist.
    let parent_handle = tree
        .locate(&parent_wiki_id)
        .map_err(|e| ApplyError::HandlerData(format!("parent wiki not found: {e}")))?;
    let new_wiki_dir = parent_handle.abs_dir().join(new_slug.as_str());
    if new_wiki_dir.exists() {
        return Err(ApplyError::InvalidPayload(format!(
            "target sub-wiki path already exists: {}",
            new_wiki_dir.display(),
        )));
    }

    let source_abs = parent_handle.abs_dir().join(&source_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let new_index_abs = new_wiki_dir.join("index.md");
    let new_index_rel = wiki::workdir_relative_source_path(tree.workdir(), &new_index_abs);

    // Validate every requested fact is active and currently lives in the
    // source page; collect them in order. Also count *every* active fact
    // on that source path so we can refuse partial moves — file →
    // sub-wiki requires moving the whole file.
    let active_on_source = fact_index::find_active_by_source_path(pool, &source_rel)
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
    let requested_set: HashSet<FactId> = fact_ids.iter().cloned().collect();
    if requested_set.len() != active_on_source.len()
        || active_on_source
            .iter()
            .any(|row| !requested_set.contains(&row.fact_id))
    {
        return Err(ApplyError::InvalidPayload(format!(
            "file_to_subwiki requires moving every active fact on {source_rel}; \
             requested {requested}, active on disk {active}. \
             Use the paragraph_to_file variant for partial moves.",
            requested = requested_set.len(),
            active = active_on_source.len(),
        )));
    }

    // Read source page contents verbatim — this becomes the new index.md.
    let source_bytes = std::fs::read_to_string(&source_abs)
        .map_err(|e| ApplyError::HandlerIo(format!("read {source_rel}: {e}")))?;

    // Sanity-parse to confirm every requested fact_id is present as a
    // marker on disk.
    let parsed = parser::parse(&source_bytes);
    let mut seen: HashSet<FactId> = HashSet::new();
    for ev in parsed.events {
        if let ParseEvent::Region { attrs, .. } = ev
            && let Some(fid) = attrs.fact_id
        {
            seen.insert(fid);
        }
    }
    for fid in &fact_ids {
        if !seen.contains(fid) {
            return Err(ApplyError::HandlerData(format!(
                "fact {fid} not present as a marker in {source_rel}",
            )));
        }
    }

    // Build the new sub-wiki's _meta.md.
    let new_title = ans
        .new_wiki_title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| new_slug.as_str())
        .to_owned();
    // Stamp the emergence-decided _meta defaults (see the
    // [memory model](../../../wiki/concepts/memory-model.md)): a
    // free-text `description` ("what goes in here") into
    // `extra["summary"]` (the same key the recall abstract uses) and a
    // dominant style **default** into `extra["style"]`.
    // The style is a hint, not a gate — only accept the closed palette,
    // and an absent/out-of-palette value leaves the wiki generic (no
    // default). Both feed the bidirectional root index (placement +
    // recall navigation).
    let mut extra = serde_yaml::Mapping::new();
    if let Some(desc) = context
        .get("new_wiki_description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        extra.insert(
            serde_yaml::Value::from("summary"),
            serde_yaml::Value::from(desc),
        );
    }
    if let Some(style) = context
        .get("new_wiki_style")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "prosa" | "prosa-tecnica" | "lista"))
    {
        extra.insert(
            serde_yaml::Value::from("style"),
            serde_yaml::Value::from(style),
        );
    }
    let meta = WikiMeta {
        wiki_id: new_wiki_id.clone(),
        wiki_type: DEFAULT_NEW_SUBWIKI_TYPE.to_owned(),
        parent_wiki_id: Some(parent_wiki_id.clone()),
        slug: new_slug.clone(),
        title: new_title,
        // Owner derives from the parent chain (this is a child of
        // `parent_wiki_id`); 2b will fill `scope` prose at emergence.
        scope: None,
        shared_with: Vec::new(),
        style_overrides: serde_yaml::Mapping::new(),
        keywords: serde_yaml::Mapping::new(),
        children: Vec::new(),
        promoted_from: Some(source_rel.clone()),
        no_archive: false,
        smart: false,
        is_agent: false,
        created: Some(chrono::Utc::now().to_rfc3339()),
        updated: None,
        extra,
    };
    // Materialise the sub-wiki (dir + _meta.md + index.md) via the shared
    // filesystem primitive. file_to_subwiki always lands a child under an
    // existing parent and never needs the child-only gate, so requires_parent
    // is false. Then move fact_index rows (wiki_id + source_path) and delete
    // the source file.
    wiki::write_wiki_dir(tree, &meta, &source_bytes, /* requires_parent */ false)
        .map_err(|e| ApplyError::HandlerIo(format!("create sub-wiki {new_wiki_id}: {e}")))?;

    for fid in &fact_ids {
        let row = fact_index::find_by_id(pool, fid)
            .await
            .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
            .ok_or_else(|| ApplyError::HandlerData(format!("fact {fid} vanished mid-apply")))?;
        // Cross-wiki move: wiki_id + source_path + offsets in one atomic
        // statement (move_region never touches wiki_id). The fact keeps
        // its byte offsets — the verbatim source bytes become the new
        // sub-wiki's index.md, so the region position is unchanged.
        let touched = fact_index::move_to_wiki(
            pool,
            fid,
            new_wiki_id.as_str(),
            &new_index_rel,
            row.region_start,
            row.region_end,
        )
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(ApplyError::HandlerData(format!(
                "fact_index::move_to_wiki updated 0 rows for {fid}",
            )));
        }
    }

    std::fs::remove_file(&source_abs)
        .map_err(|e| ApplyError::HandlerIo(format!("remove {source_rel}: {e}")))?;

    // Plan-sync seam: the emergence is a plan move too. The old page
    // leaves the persisted plan and the facts re-home onto the emerged
    // wiki's index page — otherwise the next carry-over would keep
    // claiming them for the parent page (and the compiler's pre-point
    // would drag the rows back the moment that page went dirty).
    let index_seed = crate::planner::RehomePageSeed::wiki_index(
        new_wiki_id.as_str(),
        &meta.title,
        context
            .get("new_wiki_description")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let old_slug = plan_slug_of_page(&ctx.source_wiki_id, &source_page_path.to_string_lossy());
    rehome_rows_with_seed(pool, &fact_ids, &index_seed, &[old_slug], tree).await;

    tracing::info!(
        parent_wiki_id = parent_wiki_id.as_str(),
        new_wiki_id = new_wiki_id.as_str(),
        source = source_rel,
        moved = fact_ids.len(),
        "promote: file_to_subwiki applied",
    );

    let spec = SubwikiSpec {
        variant: VARIANT_FILE_TO_SUBWIKI.to_owned(),
        source_wiki_id: ctx.source_wiki_id,
        source_page: source_page_path.to_string_lossy().into_owned(),
        source_page_bytes: source_bytes,
        new_wiki_id: new_wiki_id.as_str().to_owned(),
        new_wiki_slug: new_slug.as_str().to_owned(),
        fact_ids: fact_ids.iter().map(|f| f.as_str().to_owned()).collect(),
    };
    Ok(json!(spec))
}

/// Revert a previously-applied file → sub-wiki promote.
///
/// The revert is conservative: it refuses to delete the new sub-wiki
/// if the user has touched it between apply and revert (added files,
/// added markers, etc.). The exact rules:
///
/// 1. The new wiki directory must contain exactly two entries
///    (`_meta.md` and `index.md`) and nothing else.
/// 2. The set of `fact_id` markers in `index.md` must match exactly
///    the `fact_ids` recorded in the spec.
/// 3. The parent's source page (`<parent_abs_dir>/<source_page>`)
///    must not exist (we are about to recreate it). If it does, the
///    operator manually re-created the file and we refuse to clobber.
///
/// On success the source page is rewritten with the verbatim bytes
/// recorded in the spec, `fact_index` rows are moved back, and the
/// sub-wiki directory is removed.
#[allow(
    clippy::too_many_lines,
    reason = "linear revert pipeline; splitting hides the order"
)]
async fn revert_file_to_subwiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let spec: SubwikiSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a SubwikiSpec: {e}")))?;
    if spec.variant != VARIANT_FILE_TO_SUBWIKI {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {} is not {VARIANT_FILE_TO_SUBWIKI}",
            spec.variant,
        )));
    }

    let parent_wiki_id = WikiId::parse(&spec.source_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.source_wiki_id invalid: {e}")))?;
    let new_wiki_id = WikiId::parse(&spec.new_wiki_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.new_wiki_id invalid: {e}")))?;
    let source_page_path = validated_page_path_rev(&spec.source_page, "spec.source_page")?;

    let parent_handle = tree
        .locate(&parent_wiki_id)
        .map_err(|e| RevertError::HandlerData(format!("parent wiki not found: {e}")))?;
    let new_wiki_dir = parent_handle.abs_dir().join(&spec.new_wiki_slug);
    let new_meta_abs = new_wiki_dir.join("_meta.md");
    let new_index_abs = new_wiki_dir.join("index.md");
    let source_abs = parent_handle.abs_dir().join(&source_page_path);
    let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &source_abs);
    let new_index_rel = wiki::workdir_relative_source_path(tree.workdir(), &new_index_abs);

    // 1. Sub-wiki dir is pristine: exactly _meta.md + index.md.
    let entries: Vec<PathBuf> = std::fs::read_dir(&new_wiki_dir)
        .map_err(|e| {
            RevertError::HandlerIo(format!("read {dir}: {e}", dir = new_wiki_dir.display()))
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    let mut names: Vec<String> = entries
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect();
    names.sort();
    if names != ["_meta.md", "index.md"] {
        return Err(RevertError::HandlerData(format!(
            "sub-wiki {dir} has extra entries {names:?} — refusing to delete; clean up manually",
            dir = new_wiki_dir.display(),
        )));
    }

    // 2. Marker set on index.md matches the spec.
    let index_bytes = std::fs::read_to_string(&new_index_abs)
        .map_err(|e| RevertError::HandlerIo(format!("read {new_index_rel}: {e}")))?;
    let parsed = parser::parse(&index_bytes);
    let mut on_disk: HashSet<String> = HashSet::new();
    for ev in parsed.events {
        if let ParseEvent::Region { attrs, .. } = ev
            && let Some(fid) = attrs.fact_id
        {
            on_disk.insert(fid.as_str().to_owned());
        }
    }
    let expected: HashSet<String> = spec.fact_ids.iter().cloned().collect();
    if on_disk != expected {
        return Err(RevertError::HandlerData(format!(
            "sub-wiki {new_index_rel} marker set diverged from spec (on_disk={on_disk:?}, expected={expected:?}) — refusing to revert",
        )));
    }

    // 3. The parent source page must not exist.
    if source_abs.exists() {
        return Err(RevertError::HandlerData(format!(
            "{source_rel} already exists — refusing to clobber; remove it manually if you really want to revert",
        )));
    }

    // Rewrite the source page with the verbatim bytes recorded in spec.
    atomic_write(&source_abs, spec.source_page_bytes.as_bytes())
        .map_err(|e| RevertError::HandlerIo(format!("atomic_write {source_rel}: {e}")))?;

    // Move fact_index rows back: wiki_id → parent, source_path → source.
    for fid_str in &spec.fact_ids {
        let fid = FactId::parse(fid_str).map_err(|e| {
            RevertError::InvalidPayload(format!("spec fact_id {fid_str} invalid: {e}"))
        })?;
        let row = fact_index::find_by_id(pool, &fid)
            .await
            .map_err(|e| RevertError::HandlerIo(e.to_string()))?
            .ok_or_else(|| {
                RevertError::HandlerData(format!("fact {fid_str} vanished mid-revert"))
            })?;
        // Cross-wiki move back: wiki_id → parent, source_path → source,
        // offsets restored in one atomic statement.
        let touched = fact_index::move_to_wiki(
            pool,
            &fid,
            parent_wiki_id.as_str(),
            &source_rel,
            row.region_start,
            row.region_end,
        )
        .await
        .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
        if touched == 0 {
            return Err(RevertError::HandlerData(format!(
                "fact_index::move_to_wiki updated 0 rows for {fid_str}",
            )));
        }
    }

    // Tear down the now-orphan sub-wiki directory.
    std::fs::remove_file(&new_meta_abs)
        .map_err(|e| RevertError::HandlerIo(format!("remove _meta.md: {e}")))?;
    std::fs::remove_file(&new_index_abs)
        .map_err(|e| RevertError::HandlerIo(format!("remove index.md: {e}")))?;
    std::fs::remove_dir(&new_wiki_dir).map_err(|e| {
        RevertError::HandlerIo(format!(
            "remove sub-wiki dir {dir}: {e}",
            dir = new_wiki_dir.display()
        ))
    })?;

    // Plan-sync seam (inverse): the source page re-enters the plan with
    // its facts; the emerged wiki's index page leaves it.
    let back_ids: Vec<FactId> = spec
        .fact_ids
        .iter()
        .filter_map(|s| FactId::parse(s).ok())
        .collect();
    let source_seed = crate::planner::RehomePageSeed::concept(
        &plan_slug_of_page(&spec.source_wiki_id, &spec.source_page),
        &spec.source_wiki_id,
    );
    let emerged_slug = crate::planner::slugify(&spec.new_wiki_id);
    rehome_rows_with_seed(pool, &back_ids, &source_seed, &[emerged_slug], tree).await;

    tracing::info!(
        parent_wiki_id = parent_wiki_id.as_str(),
        new_wiki_id = new_wiki_id.as_str(),
        source = source_rel,
        moved_back = spec.fact_ids.len(),
        "promote: file_to_subwiki reverted",
    );

    Ok(())
}

// ---------- Helpers ----------

struct ParsedRegion {
    start: usize,
    end: usize,
    bytes: String,
}

struct MovedRegion {
    fact_id: FactId,
    old_start: usize,
    old_end: usize,
    bytes: String,
}

fn parse_context(v: &Value) -> Result<PromoteContext, ApplyError> {
    serde_json::from_value::<PromoteContext>(v.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))
}

/// Pull a required non-empty string field out of a JSON object, with a
/// field-named [`ApplyError::InvalidPayload`] on miss. Used by the
/// cross-wiki refile variant which reads a handful of scalar fields off
/// `context`/`answers` rather than deserialising a whole struct.
fn json_str(v: &Value, key: &str, field: &str) -> Result<String, ApplyError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            ApplyError::InvalidPayload(format!("{field} is required (non-empty string)"))
        })
}

fn parse_answers(v: &Value) -> Result<PromoteAnswers, ApplyError> {
    serde_json::from_value::<PromoteAnswers>(v.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("answers: {e}")))
}

fn parse_fact_ids(ss: &[String]) -> Result<Vec<FactId>, ApplyError> {
    if ss.is_empty() {
        return Err(ApplyError::InvalidPayload(
            "context.fact_ids must not be empty".into(),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(ss.len());
    for s in ss {
        if !seen.insert(s.clone()) {
            return Err(ApplyError::InvalidPayload(format!(
                "context.fact_ids duplicate: {s}",
            )));
        }
        out.push(
            FactId::parse(s)
                .map_err(|e| ApplyError::InvalidPayload(format!("fact_id {s} invalid: {e}")))?,
        );
    }
    Ok(out)
}

fn validated_page_path(s: &str, field: &str) -> Result<PathBuf, ApplyError> {
    let p = PathBuf::from(s);
    if !is_safe_page_path(&p) {
        return Err(ApplyError::InvalidPayload(format!(
            "{field} is not a safe page path: {s}",
        )));
    }
    Ok(p)
}

fn validated_page_path_rev(s: &str, field: &str) -> Result<PathBuf, RevertError> {
    let p = PathBuf::from(s);
    if !is_safe_page_path(&p) {
        return Err(RevertError::InvalidPayload(format!(
            "{field} is not a safe page path: {s}",
        )));
    }
    Ok(p)
}

/// Append every region in `moved` to `existing` and return both the
/// composed string and a per-fact_id map of `(new_start, new_end)`
/// byte offsets in the composed result.
fn compose_target(
    existing: &str,
    moved: &[MovedRegion],
) -> (String, HashMap<FactId, (usize, usize)>) {
    let mut out = String::with_capacity(
        existing.len() + moved.iter().map(|m| m.bytes.len() + 1).sum::<usize>(),
    );
    out.push_str(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let mut offsets = HashMap::with_capacity(moved.len());
    for m in moved {
        let start = out.len();
        out.push_str(&m.bytes);
        let end = out.len();
        offsets.insert(m.fact_id.clone(), (start, end));
        out.push('\n');
    }
    (out, offsets)
}

/// Return `source` with every byte range in `moved` (sorted by start)
/// excised. Adjacent newlines around the excised spans are preserved
/// verbatim — we trade slightly suboptimal whitespace for a deterministic
/// move that round-trips losslessly through revert.
fn compose_source_minus_moved(source: &str, moved: &[MovedRegion]) -> String {
    let mut spans: Vec<(usize, usize)> = moved.iter().map(|m| (m.old_start, m.old_end)).collect();
    spans.sort_by_key(|&(s, _)| s);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (s, e) in spans {
        out.push_str(&source[cursor..s]);
        cursor = e;
    }
    out.push_str(&source[cursor..]);
    out
}

// ---------- Direct apply (act-first) ----------

/// Errors from the act-first promote path.
///
/// Distinguishes "the apply itself failed (nothing changed on disk)"
/// from "the change IS applied but the undo receipt could not be
/// recorded" — callers log the second loudly instead of retrying the
/// apply.
#[derive(Debug, thiserror::Error)]
pub enum DirectPromoteError {
    /// The apply handler refused or failed; no receipt was written.
    #[error("{0}")]
    Apply(#[from] ApplyError),
    /// The structural change applied, but inserting the born-applied
    /// receipt failed. The REM WAL op is the remaining audit trail.
    #[error("change applied but undo receipt failed: {0}")]
    Receipt(#[from] ProposalsError),
}

/// Receipt of a direct (act-first) structural apply: the born-applied
/// undo row plus the spec the notice event reads.
#[derive(Debug, Clone)]
pub struct DirectApplied {
    /// `structure_proposals` row id of the born-applied receipt — the
    /// undo anchor the dashboard revert path reads the token from.
    pub proposal_id: String,
    /// Instant the undo window closes.
    pub revert_deadline: chrono::DateTime<chrono::Utc>,
    /// Spec returned by the apply handler (the `PromoteSpec` /
    /// `SubwikiSpec` shape) — carries the concrete target
    /// (`target_page` / `new_wiki_id`).
    pub spec: Value,
}

/// Metadata for a `wiki_promote` receipt of variant `paragraph_to_file`.
///
/// The REM auto-promotion path attaches these hints for
/// dashboard presentation. The handler only reads `fact_ids` +
/// `source_*` + `target_page` from context/answers; the rest is pure
/// presentation.
#[derive(Debug, Clone, Default)]
pub struct ParagraphToFileHints {
    /// Page mass the emitter recorded for the candidate — the number of
    /// active facts sharing the fact's page when the promotion fired.
    /// Surfaced for operator audit; the trigger is mass/ramification,
    /// not a single fact's word count (see the
    /// [memory model](../../../wiki/concepts/memory-model.md)).
    pub trigger_page_facts: Option<usize>,
    /// Recall hits in the last 30 days, when known.
    pub recall_count_30d: Option<i64>,
    /// Free-form reason string ("REM page mass 9 facts + 7 recall hits").
    pub reason: Option<String>,
}

/// Build the canonical question array stored on a paragraph→file
/// receipt. Display-only since the act-first conversion: the dashboard
/// renders it to show *what* was decided (the target page), there is no
/// approval step that reads it back.
fn paragraph_to_file_questions(recommended_target: &str) -> Value {
    json!([{
        "id": "target_page",
        "text": "Where should the paragraph live?",
        "options": [{
            "id": "use_recommended",
            "label": format!("Move to `{recommended_target}`"),
            "value": recommended_target,
            "recommended": true,
        }]
    }])
}

/// Context JSON shared by the `paragraph_to_file` receipt — the same
/// shape the apply handler reads and the dashboard renders.
fn paragraph_to_file_context(
    source_wiki_id: &str,
    source_page: &str,
    fact_ids: &[FactId],
    target_page: &str,
    hints: &ParagraphToFileHints,
) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("source_wiki_id".into(), json!(source_wiki_id));
    context.insert("source_page".into(), json!(source_page));
    context.insert(
        "fact_ids".into(),
        json!(fact_ids.iter().map(FactId::as_str).collect::<Vec<_>>()),
    );
    context.insert("variant".into(), json!(VARIANT_PARAGRAPH_TO_FILE));
    context.insert("recommended_target_page".into(), json!(target_page));
    if let Some(m) = hints.trigger_page_facts {
        context.insert("trigger_page_facts".into(), json!(m));
    }
    if let Some(r) = hints.recall_count_30d {
        context.insert("recall_count_30d".into(), json!(r));
    }
    if let Some(r) = &hints.reason {
        context.insert("reason".into(), json!(r));
    }
    Value::Object(context)
}

/// Apply a paragraph→file move **directly** (act-first).
///
/// Runs the `paragraph_to_file` handler now, then records a
/// **born-applied** `wiki_promote` receipt with an open revert window.
/// There is no `pending` stage and no approval step — the caller (REM)
/// emits the `structure_applied` notice naming the affected user; the
/// dashboard is the *undo* surface, not an approval surface.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler fails (nothing
/// changed); [`DirectPromoteError::Receipt`] when the change applied
/// but the undo receipt could not be written.
pub async fn apply_paragraph_to_file_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    source_wiki_id: &str,
    source_page: &str,
    fact_ids: &[FactId],
    target_page: &str,
    hints: &ParagraphToFileHints,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context =
        paragraph_to_file_context(source_wiki_id, source_page, fact_ids, target_page, hints);
    let answers = json!({ "target_page": target_page });
    let spec = apply_paragraph_to_file(pool, tree, &context, &answers).await?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            context,
            paragraph_to_file_questions(target_page),
        )
        .with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

/// Display-only question array stored on a `fact_refile` receipt.
fn fact_refile_questions(dest_wiki_id: &str, dest_page: &str) -> Value {
    json!([{
        "id": "dest_wiki_id",
        "text": "Which wiki does this fact belong in?",
        "options": [{
            "id": "use_recommended",
            "label": format!("Move to `{dest_wiki_id}` · `{dest_page}`"),
            "value": dest_wiki_id,
            "recommended": true,
        }]
    }])
}

/// Context JSON shared by the `fact_refile` receipt — the same shape the
/// apply handler reads and the dashboard renders.
fn fact_refile_context(
    fact_id: &FactId,
    source_wiki_id: &str,
    source_page: &str,
    dest_wiki_id: &str,
    dest_page: &str,
    reason: Option<&str>,
) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("variant".into(), json!(VARIANT_FACT_REFILE));
    context.insert("fact_id".into(), json!(fact_id.as_str()));
    context.insert("source_wiki_id".into(), json!(source_wiki_id));
    context.insert("source_page".into(), json!(source_page));
    context.insert("recommended_dest_wiki_id".into(), json!(dest_wiki_id));
    context.insert("recommended_dest_page".into(), json!(dest_page));
    if let Some(r) = reason {
        context.insert("reason".into(), json!(r));
    }
    Value::Object(context)
}

/// Move **one** fact to a **different** existing wiki **directly**
/// (act-first) — the REM cross-wiki refile verb.
///
/// Runs the `fact_refile` handler now, then records a **born-applied**
/// `wiki_promote` receipt with an open revert window (the 7-day
/// dashboard undo). No `pending` stage, no approval: the caller (REM)
/// emits the `structure_applied` notice; the dashboard is the undo
/// surface, not an approval surface. `reason` is a one-line audit string
/// for the receipt (e.g. the LLM's stated rationale).
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler refuses or fails
/// (nothing changed on disk); [`DirectPromoteError::Receipt`] when the
/// move applied but the undo receipt could not be written.
#[allow(
    clippy::too_many_arguments,
    reason = "the cross-wiki refile carries both endpoints (fact, source wiki/page, dest wiki/page) + reason + recipient; bundling into a struct would just hide the same fields, as in apply_file_to_subwiki_direct"
)]
pub async fn apply_fact_refile_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    fact_id: &FactId,
    source_wiki_id: &str,
    source_page: &str,
    dest_wiki_id: &str,
    dest_page: &str,
    reason: Option<&str>,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = fact_refile_context(
        fact_id,
        source_wiki_id,
        source_page,
        dest_wiki_id,
        dest_page,
        reason,
    );
    let answers = json!({
        "variant": VARIANT_FACT_REFILE,
        "dest_wiki_id": dest_wiki_id,
        "dest_page": dest_page,
    });
    let spec = apply_fact_refile(pool, tree, &context, &answers).await?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            context,
            fact_refile_questions(dest_wiki_id, dest_page),
        )
        .with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

/// Move **one** fact cross-wiki **without** minting its own receipt — the
/// bundle building block.
///
/// Like [`apply_fact_refile_direct`] (same handler, same on-disk effect) but
/// it returns the `fact_refile` spec instead of emitting a born-applied
/// receipt. The governed page-deletion bundle ([`crate::page::delete_page_direct`])
/// evacuates each foreign-authored fact through this and folds the returned
/// specs into the **single** `bundle` receipt, so the whole page deletion is
/// one revertible unit. The returned spec is exactly what
/// [`revert_wiki_promote`] (`fact_refile` variant) reads to undo the move, so a
/// [`crate::bundle::revert_bundle`] can replay it.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the move handler refuses or fails
/// (nothing changed on disk).
pub(crate) async fn apply_fact_refile_collect(
    pool: &SqlitePool,
    tree: &WikiTree,
    fact_id: &FactId,
    source_wiki_id: &str,
    source_page: &str,
    dest_wiki_id: &str,
    dest_page: &str,
    reason: Option<&str>,
) -> Result<Value, DirectPromoteError> {
    let context = fact_refile_context(
        fact_id,
        source_wiki_id,
        source_page,
        dest_wiki_id,
        dest_page,
        reason,
    );
    let answers = json!({
        "variant": VARIANT_FACT_REFILE,
        "dest_wiki_id": dest_wiki_id,
        "dest_page": dest_page,
    });
    Ok(apply_fact_refile(pool, tree, &context, &answers).await?)
}

/// Inputs of [`apply_page_merge_direct`] — the husk + survivor identity the
/// REM merge sub-job resolved from the compilation plan, plus presentation
/// hints for the receipt.
#[derive(Debug, Clone)]
pub struct PageMergeParams<'a> {
    /// The standard wiki the HUSK page lives in.
    pub wiki_id: &'a str,
    /// The standard wiki the SURVIVOR page lives in — usually the same
    /// as [`Self::wiki_id`], but a family-scope merge may cross the
    /// parent↔sub-wiki line (never an arbitrary wiki pair).
    pub survivor_wiki_id: &'a str,
    /// The husk page (wiki-relative `.md`) — loses all facts, gets deleted.
    pub husk_page: &'a str,
    /// The survivor page (wiki-relative `.md`) — gains the facts.
    pub survivor_page: &'a str,
    /// Every active fact of the husk (the handler refuses partial moves).
    pub fact_ids: &'a [FactId],
    /// Husk identity stored for the revert's plan re-seed.
    pub husk_title: &'a str,
    /// See [`Self::husk_title`].
    pub husk_description: &'a str,
    /// See [`Self::husk_title`].
    pub husk_style: Option<&'a str>,
    /// One-line reason for the receipt ("LLM confirmed same concept: …").
    pub reason: Option<String>,
}

/// Display-only question array stored on a page-merge receipt.
fn page_merge_questions(husk: &str, survivor: &str) -> Value {
    json!([{
        "id": "target_page",
        "text": format!("Merge `{husk}` into `{survivor}`?"),
        "options": [{
            "id": "use_recommended",
            "label": format!("Move every fact to `{survivor}` and delete `{husk}`"),
            "value": survivor,
            "recommended": true,
        }]
    }])
}

/// Context JSON shared by the page-merge receipt — the same shape the
/// apply handler reads and the dashboard renders.
fn page_merge_context(p: &PageMergeParams<'_>) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("source_wiki_id".into(), json!(p.wiki_id));
    context.insert("target_wiki_id".into(), json!(p.survivor_wiki_id));
    context.insert("source_page".into(), json!(p.husk_page));
    context.insert(
        "fact_ids".into(),
        json!(p.fact_ids.iter().map(FactId::as_str).collect::<Vec<_>>()),
    );
    context.insert("variant".into(), json!(VARIANT_PAGE_MERGE));
    context.insert("recommended_target_page".into(), json!(p.survivor_page));
    context.insert("husk_title".into(), json!(p.husk_title));
    context.insert("husk_description".into(), json!(p.husk_description));
    if let Some(s) = p.husk_style {
        context.insert("husk_style".into(), json!(s));
    }
    if let Some(r) = &p.reason {
        context.insert("reason".into(), json!(r));
    }
    Value::Object(context)
}

/// Apply a page merge **directly** (act-first).
///
/// Runs the `page_merge` handler now — every fact of the husk moves onto
/// the survivor, the husk file is deleted, the persisted plan is re-homed —
/// then records a **born-applied** `wiki_promote` receipt with an open
/// revert window. No `pending` stage, no approval: the caller (REM) emits
/// the `structure_applied` notice; the dashboard is the undo surface.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler fails (nothing changed);
/// [`DirectPromoteError::Receipt`] when the merge applied but the undo
/// receipt could not be written.
pub async fn apply_page_merge_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    params: &PageMergeParams<'_>,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = page_merge_context(params);
    let answers = json!({
        "variant": VARIANT_PAGE_MERGE,
        "target_page": params.survivor_page,
    });
    let spec = apply_page_merge(pool, tree, &context, &answers).await?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            context,
            page_merge_questions(params.husk_page, params.survivor_page),
        )
        .with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

// ---------- The validity_close variant (born-applied closures) ----------

/// One closed target inside a `validity_close` receipt's spec — what was
/// stamped and the snapshot the revert restores.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClosureRecord {
    fact_id: String,
    /// `valid_to` the closure stamped.
    valid_to: String,
    /// `decay_reason` the closure stamped (`fact_index::decay` vocabulary).
    reason: String,
    /// `valid_to` before the closure (`None` = the window was open).
    prev_valid_to: Option<String>,
    /// `decay_reason` before the closure.
    prev_decay_reason: Option<String>,
    /// `successor_fact_id` before the closure (`None` on receipts written
    /// before the successor pointer existed — deserialized as absent).
    #[serde(default)]
    prev_successor_fact_id: Option<String>,
}

/// `spec` payload of a `validity_close` receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidityCloseSpec {
    variant: String,
    closures: Vec<ClosureRecord>,
}

/// Where an applied closure landed — a promoted fact row or a
/// still-buffered capture.
///
/// The revert does not need it (it probes the fact first, then the
/// buffer — the id is stable across promotion), but the receipt records
/// it for the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosureSurface {
    /// The target was a promoted `fact_index` row.
    Fact,
    /// The target was a still-buffered capture (same-day flow).
    Buffer,
}

impl ClosureSurface {
    /// Stable receipt/log string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Buffer => "buffer",
        }
    }
}

/// One applied closure as the ingest orchestrator reports it for the
/// born-applied receipt.
#[derive(Debug, Clone)]
pub struct AppliedClosure {
    /// The closed target.
    pub fact_id: FactId,
    /// Wiki the target lives in (for the audit context / the notice).
    pub wiki_id: String,
    /// Short claim preview shown on the dashboard receipt.
    pub preview: String,
    /// `valid_to` stamped.
    pub valid_to: String,
    /// `decay_reason` stamped.
    pub reason: String,
    /// Snapshot taken at closure time — the revert payload.
    pub prev: fact_index::ClosedValidity,
    /// Which surface the closure landed on.
    pub surface: ClosureSurface,
}

/// Display-only question array stored on a `validity_close` receipt.
fn validity_close_questions(closures: &[AppliedClosure]) -> Value {
    let lines: Vec<String> = closures
        .iter()
        .map(|c| format!("{} ({})", c.preview, c.reason))
        .collect();
    json!([{
        "id": "closures",
        "text": format!("Close the validity of {} fact(s)?", closures.len()),
        "options": [{
            "id": "use_recommended",
            "label": lines.join(" · "),
            "value": "close",
            "recommended": true,
        }]
    }])
}

/// Context JSON stored on a `validity_close` receipt — what the
/// dashboard renders.
fn validity_close_context(closures: &[AppliedClosure], gesture: Option<&str>) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("variant".into(), json!(VARIANT_VALIDITY_CLOSE));
    context.insert(
        "closed".into(),
        json!(
            closures
                .iter()
                .map(|c| {
                    json!({
                        "fact_id": c.fact_id.as_str(),
                        "wiki_id": c.wiki_id,
                        "preview": c.preview,
                        "reason": c.reason,
                        "valid_to": c.valid_to,
                        "surface": c.surface.as_str(),
                    })
                })
                .collect::<Vec<_>>()
        ),
    );
    if let Some(g) = gesture {
        context.insert("gesture".into(), json!(g));
    }
    Value::Object(context)
}

/// Record a batch of already-applied validity closures as one
/// **born-applied** `wiki_promote` receipt (variant `validity_close`).
///
/// The ingest orchestrator has already stamped every target
/// (`fact_index::close_validity` / `capture_buffer::close_validity`);
/// this writes the undoable receipt with the open revert window — the
/// act-first pattern: the caller emits the `structure_applied` notice and
/// the dashboard is the undo surface.
///
/// `gesture` is a short preview of the user message that triggered the
/// closures (audit/display only). `applied_by` is the sender's raw id.
///
/// # Errors
///
/// [`DirectPromoteError::Receipt`] when the receipt row cannot be
/// written (the closures themselves are already applied).
pub async fn emit_validity_close_receipt(
    pool: &SqlitePool,
    closures: &[AppliedClosure],
    gesture: Option<&str>,
    applied_by: Option<&str>,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let spec = serde_json::to_value(ValidityCloseSpec {
        variant: VARIANT_VALIDITY_CLOSE.to_owned(),
        closures: closures
            .iter()
            .map(|c| ClosureRecord {
                fact_id: c.fact_id.as_str().to_owned(),
                valid_to: c.valid_to.clone(),
                reason: c.reason.clone(),
                prev_valid_to: c.prev.prev_valid_to.clone(),
                prev_decay_reason: c.prev.prev_decay_reason.clone(),
                prev_successor_fact_id: c
                    .prev
                    .prev_successor_fact_id
                    .as_ref()
                    .map(|f| f.as_str().to_owned()),
            })
            .collect(),
    })
    .map_err(|e| DirectPromoteError::Receipt(proposals::ProposalsError::Json(e)))?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            validity_close_context(closures, gesture),
            validity_close_questions(closures),
        )
        .with_recipient(recipient),
        spec.clone(),
        applied_by,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

/// Revert a `validity_close` receipt: restore every target's validity
/// snapshot.
///
/// Probes the fact row first, then the still-buffered capture — the id
/// is stable across promotion, so whichever surface holds the target now
/// gets the restore. A target that vanished in the meantime (tombstoned,
/// journal wiped) is logged and skipped: the revert restores what it
/// can rather than failing the batch.
async fn revert_validity_close(pool: &SqlitePool, spec: &Value) -> Result<(), RevertError> {
    let spec: ValidityCloseSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("bad validity_close spec: {e}")))?;
    if spec.variant != VARIANT_VALIDITY_CLOSE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {} is not {VARIANT_VALIDITY_CLOSE}",
            spec.variant
        )));
    }
    for c in &spec.closures {
        let fact_id = FactId::parse(&c.fact_id)
            .map_err(|e| RevertError::InvalidPayload(format!("bad fact_id in spec: {e}")))?;
        let touched = fact_index::restore_validity(
            pool,
            &fact_id,
            c.prev_valid_to.as_deref(),
            c.prev_decay_reason.as_deref(),
            c.prev_successor_fact_id.as_deref(),
        )
        .await
        .map_err(|e| RevertError::HandlerData(format!("fact restore: {e}")))?;
        if touched > 0 {
            continue;
        }
        let buffered = capture_buffer::restore_validity(
            pool,
            &fact_id,
            c.prev_valid_to.as_deref(),
            c.prev_decay_reason.as_deref(),
        )
        .await
        .map_err(|e| RevertError::HandlerData(format!("buffer restore: {e}")))?;
        if buffered == 0 {
            tracing::warn!(
                fact_id = c.fact_id,
                "promote: validity_close revert target vanished — skipped"
            );
        }
    }
    tracing::info!(
        closures = spec.closures.len(),
        "promote: validity_close reverted"
    );
    Ok(())
}

// ---------- The validity_edit variant (born-applied date corrections) ----------

/// One edited target inside a `validity_edit` receipt's spec — the new
/// interval and the snapshot the revert restores.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidityEditRecord {
    fact_id: String,
    /// `valid_from` the edit set (`None` = left unchanged).
    new_valid_from: Option<String>,
    /// `valid_to` the edit set (`None` = left unchanged).
    new_valid_to: Option<String>,
    /// `valid_from` before the edit (the revert restores it verbatim).
    prev_valid_from: Option<String>,
    /// `valid_to` before the edit.
    prev_valid_to: Option<String>,
}

/// `spec` payload of a `validity_edit` receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidityEditSpec {
    variant: String,
    edits: Vec<ValidityEditRecord>,
}

/// One applied validity edit as the ingest orchestrator reports it for the
/// born-applied receipt.
#[derive(Debug, Clone)]
pub struct AppliedValidityEdit {
    /// The edited target.
    pub fact_id: FactId,
    /// Wiki the target lives in (for the audit context / the notice).
    pub wiki_id: String,
    /// Short claim preview shown on the dashboard receipt.
    pub preview: String,
    /// `valid_from` the edit set (`None` = left unchanged).
    pub new_valid_from: Option<String>,
    /// `valid_to` the edit set (`None` = left unchanged).
    pub new_valid_to: Option<String>,
    /// Snapshot taken at edit time — the revert payload.
    pub prev: fact_index::PrevValidity,
    /// Which surface the edit landed on.
    pub surface: ClosureSurface,
}

/// Display-only question array stored on a `validity_edit` receipt.
fn validity_edit_questions(edits: &[AppliedValidityEdit]) -> Value {
    let lines: Vec<String> = edits.iter().map(|e| e.preview.clone()).collect();
    json!([{
        "id": "validity_edits",
        "text": format!("Correct the validity dates of {} fact(s)?", edits.len()),
        "options": [{
            "id": "use_recommended",
            "label": lines.join(" · "),
            "value": "edit",
            "recommended": true,
        }]
    }])
}

/// Context JSON stored on a `validity_edit` receipt — what the dashboard
/// renders (old → new bounds per fact).
fn validity_edit_context(edits: &[AppliedValidityEdit], gesture: Option<&str>) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("variant".into(), json!(VARIANT_VALIDITY_EDIT));
    context.insert(
        "edited".into(),
        json!(
            edits
                .iter()
                .map(|e| {
                    json!({
                        "fact_id": e.fact_id.as_str(),
                        "wiki_id": e.wiki_id,
                        "preview": e.preview,
                        "new_valid_from": e.new_valid_from,
                        "new_valid_to": e.new_valid_to,
                        "prev_valid_from": e.prev.prev_valid_from,
                        "prev_valid_to": e.prev.prev_valid_to,
                        "surface": e.surface.as_str(),
                    })
                })
                .collect::<Vec<_>>()
        ),
    );
    if let Some(g) = gesture {
        context.insert("gesture".into(), json!(g));
    }
    Value::Object(context)
}

/// Record a batch of already-applied validity-date corrections as one
/// **born-applied** `wiki_promote` receipt (variant `validity_edit`).
///
/// The sibling of [`emit_validity_close_receipt`], for a *correction* of
/// the dates rather than a completion/retraction: the ingest orchestrator
/// has already set every target's interval
/// ([`fact_index::set_validity`] / [`capture_buffer::set_validity`]);
/// this writes the undoable receipt with the open revert window.
///
/// # Errors
///
/// [`DirectPromoteError::Receipt`] when the receipt row cannot be written
/// (the edits themselves are already applied).
pub async fn emit_validity_edit_receipt(
    pool: &SqlitePool,
    edits: &[AppliedValidityEdit],
    gesture: Option<&str>,
    applied_by: Option<&str>,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let spec = serde_json::to_value(ValidityEditSpec {
        variant: VARIANT_VALIDITY_EDIT.to_owned(),
        edits: edits
            .iter()
            .map(|e| ValidityEditRecord {
                fact_id: e.fact_id.as_str().to_owned(),
                new_valid_from: e.new_valid_from.clone(),
                new_valid_to: e.new_valid_to.clone(),
                prev_valid_from: e.prev.prev_valid_from.clone(),
                prev_valid_to: e.prev.prev_valid_to.clone(),
            })
            .collect(),
    })
    .map_err(|e| DirectPromoteError::Receipt(proposals::ProposalsError::Json(e)))?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            validity_edit_context(edits, gesture),
            validity_edit_questions(edits),
        )
        .with_recipient(recipient),
        spec.clone(),
        applied_by,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

/// Revert a `validity_edit` receipt: restore every target's interval
/// snapshot (both bounds), leaving `decay_reason` untouched.
///
/// Probes the fact row first, then the still-buffered capture — the id is
/// stable across promotion. A vanished target is logged and skipped.
async fn revert_validity_edit(pool: &SqlitePool, spec: &Value) -> Result<(), RevertError> {
    let spec: ValidityEditSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("bad validity_edit spec: {e}")))?;
    if spec.variant != VARIANT_VALIDITY_EDIT {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {} is not {VARIANT_VALIDITY_EDIT}",
            spec.variant
        )));
    }
    for e in &spec.edits {
        let fact_id = FactId::parse(&e.fact_id)
            .map_err(|err| RevertError::InvalidPayload(format!("bad fact_id in spec: {err}")))?;
        let touched = fact_index::restore_validity_interval(
            pool,
            &fact_id,
            e.prev_valid_from.as_deref(),
            e.prev_valid_to.as_deref(),
        )
        .await
        .map_err(|err| RevertError::HandlerData(format!("fact restore: {err}")))?;
        if touched > 0 {
            continue;
        }
        let buffered = capture_buffer::restore_validity_interval(
            pool,
            &fact_id,
            e.prev_valid_from.as_deref(),
            e.prev_valid_to.as_deref(),
        )
        .await
        .map_err(|err| RevertError::HandlerData(format!("buffer restore: {err}")))?;
        if buffered == 0 {
            tracing::warn!(
                fact_id = e.fact_id,
                "promote: validity_edit revert target vanished — skipped"
            );
        }
    }
    tracing::info!(edits = spec.edits.len(), "promote: validity_edit reverted");
    Ok(())
}

// ---------- The acl_change variant (born-applied ACL changes) ----------

/// One changed target inside an `acl_change` receipt's spec — the new ACL,
/// the snapshot the revert restores, and the audit row to mark reverted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AclChangeRecord {
    fact_id: String,
    /// New ACL, principals as wire strings.
    new_owner_id: String,
    new_allow_ids: Vec<String>,
    new_sender_id: Option<String>,
    /// Previous ACL (the revert restores it verbatim).
    prev_owner_id: String,
    prev_allow_ids: Vec<String>,
    prev_sender_id: Option<String>,
    /// `disclosure_audit.audit_id` the change wrote — marked reverted on
    /// undo.
    audit_id: i64,
    /// Whether the change widened the effective read-set.
    widening: bool,
}

/// `spec` payload of an `acl_change` receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AclChangeSpec {
    variant: String,
    changes: Vec<AclChangeRecord>,
}

/// One applied ACL change as the ingest orchestrator reports it for the
/// born-applied receipt.
#[derive(Debug, Clone)]
pub struct AppliedAclChange {
    /// The changed target.
    pub fact_id: FactId,
    /// Wiki the target lives in (for the audit context / the notice).
    pub wiki_id: String,
    /// Short claim preview shown on the dashboard receipt.
    pub preview: String,
    /// New owner.
    pub new_owner: Principal,
    /// New allow-list.
    pub new_allow: Vec<Principal>,
    /// Snapshot taken at change time — the revert payload.
    pub prev: fact_index::PrevAcl,
    /// `disclosure_audit.audit_id` the change wrote.
    pub audit_id: i64,
    /// Whether the change widened the effective read-set.
    pub widening: bool,
    /// Which surface the change landed on.
    pub surface: ClosureSurface,
}

fn principal_strings(ps: &[Principal]) -> Vec<String> {
    ps.iter().map(ToString::to_string).collect()
}

/// Display-only question array stored on an `acl_change` receipt.
fn acl_change_questions(changes: &[AppliedAclChange]) -> Value {
    let lines: Vec<String> = changes.iter().map(|c| c.preview.clone()).collect();
    json!([{
        "id": "acl_changes",
        "text": format!("Change the sharing of {} fact(s)?", changes.len()),
        "options": [{
            "id": "use_recommended",
            "label": lines.join(" · "),
            "value": "change",
            "recommended": true,
        }]
    }])
}

/// Context JSON stored on an `acl_change` receipt — what the dashboard
/// renders (old → new read-set per fact, plus the widening flag).
fn acl_change_context(changes: &[AppliedAclChange], gesture: Option<&str>) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("variant".into(), json!(VARIANT_ACL_CHANGE));
    context.insert(
        "changed".into(),
        json!(
            changes
                .iter()
                .map(|c| {
                    json!({
                        "fact_id": c.fact_id.as_str(),
                        "wiki_id": c.wiki_id,
                        "preview": c.preview,
                        "new_owner_id": c.new_owner.to_string(),
                        "new_allow_ids": principal_strings(&c.new_allow),
                        "prev_owner_id": c.prev.prev_owner_id.to_string(),
                        "prev_allow_ids": principal_strings(&c.prev.prev_allow_ids),
                        "widening": c.widening,
                        "audit_id": c.audit_id,
                        "surface": c.surface.as_str(),
                    })
                })
                .collect::<Vec<_>>()
        ),
    );
    if let Some(g) = gesture {
        context.insert("gesture".into(), json!(g));
    }
    Value::Object(context)
}

/// Record a batch of already-applied per-fact ACL changes as one
/// **born-applied** `wiki_promote` receipt (variant `acl_change`).
///
/// The sibling of [`emit_validity_close_receipt`], for a sharing change:
/// the ingest orchestrator has already stamped every target's ACL
/// ([`fact_index::set_acl`] / [`capture_buffer::set_acl`]) and written the
/// [`crate::disclosure_audit`] rows; this writes the undoable receipt.
///
/// # Errors
///
/// [`DirectPromoteError::Receipt`] when the receipt row cannot be written
/// (the changes themselves are already applied).
pub async fn emit_acl_change_receipt(
    pool: &SqlitePool,
    changes: &[AppliedAclChange],
    gesture: Option<&str>,
    applied_by: Option<&str>,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let spec = serde_json::to_value(AclChangeSpec {
        variant: VARIANT_ACL_CHANGE.to_owned(),
        changes: changes
            .iter()
            .map(|c| AclChangeRecord {
                fact_id: c.fact_id.as_str().to_owned(),
                new_owner_id: c.new_owner.to_string(),
                new_allow_ids: principal_strings(&c.new_allow),
                // An acl-change re-shares (owner/allow) only and PRESERVES the
                // fact's cross-user attribution: `set_acl` was called with the
                // prior sender, so the applied sender equals `prev_sender_id`.
                // Record that (not None) so the receipt + disclosure audit
                // match the DB. Revert restores `prev_sender_id` regardless.
                new_sender_id: c.prev.prev_sender_id.as_ref().map(ToString::to_string),
                prev_owner_id: c.prev.prev_owner_id.to_string(),
                prev_allow_ids: principal_strings(&c.prev.prev_allow_ids),
                prev_sender_id: c.prev.prev_sender_id.as_ref().map(ToString::to_string),
                audit_id: c.audit_id,
                widening: c.widening,
            })
            .collect(),
    })
    .map_err(|e| DirectPromoteError::Receipt(proposals::ProposalsError::Json(e)))?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            acl_change_context(changes, gesture),
            acl_change_questions(changes),
        )
        .with_recipient(recipient),
        spec.clone(),
        applied_by,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

/// Revert an `acl_change` receipt: restore every target's ACL snapshot and
/// mark its disclosure-audit row reverted.
///
/// Probes the fact row first, then the still-buffered capture — the id is
/// stable across promotion. A vanished target is logged and skipped; the
/// audit row is still stamped reverted (the change is logically undone).
async fn revert_acl_change(pool: &SqlitePool, spec: &Value) -> Result<(), RevertError> {
    let spec: AclChangeSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("bad acl_change spec: {e}")))?;
    if spec.variant != VARIANT_ACL_CHANGE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {} is not {VARIANT_ACL_CHANGE}",
            spec.variant
        )));
    }
    for c in &spec.changes {
        let fact_id = FactId::parse(&c.fact_id)
            .map_err(|err| RevertError::InvalidPayload(format!("bad fact_id in spec: {err}")))?;
        let owner = c
            .prev_owner_id
            .parse::<Principal>()
            .map_err(|err| RevertError::InvalidPayload(format!("bad prev_owner_id: {err}")))?;
        let allow = c
            .prev_allow_ids
            .iter()
            .map(|s| s.parse::<Principal>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| RevertError::InvalidPayload(format!("bad prev_allow_ids: {err}")))?;
        let sender = c
            .prev_sender_id
            .as_deref()
            .map(str::parse::<Principal>)
            .transpose()
            .map_err(|err| RevertError::InvalidPayload(format!("bad prev_sender_id: {err}")))?;

        let touched = fact_index::restore_acl(pool, &fact_id, &owner, &allow, sender.as_ref())
            .await
            .map_err(|err| RevertError::HandlerData(format!("fact ACL restore: {err}")))?;
        if touched == 0 {
            let buffered =
                capture_buffer::restore_acl(pool, &fact_id, &owner, &allow, sender.as_ref())
                    .await
                    .map_err(|err| {
                        RevertError::HandlerData(format!("buffer ACL restore: {err}"))
                    })?;
            if buffered == 0 {
                tracing::warn!(
                    fact_id = c.fact_id,
                    "promote: acl_change revert target vanished — skipped"
                );
            }
        }
        // The audit row is stamped reverted regardless — the change is
        // logically undone even if the row itself has since vanished.
        crate::disclosure_audit::mark_reverted(pool, c.audit_id)
            .await
            .map_err(|err| RevertError::HandlerData(format!("audit mark reverted: {err}")))?;
    }
    tracing::info!(changes = spec.changes.len(), "promote: acl_change reverted");
    Ok(())
}

/// Metadata for a `wiki_promote` proposal of variant `file_to_subwiki`.
///
/// The REM emergence emitter attaches these hints for
/// dashboard presentation. The handler only reads `fact_ids` +
/// `source_*` from context and `new_wiki_slug`/`new_wiki_title` from
/// answers; the rest is pure presentation/audit.
#[derive(Debug, Clone, Default)]
pub struct FileToSubwikiHints {
    /// Page mass that tripped the emergence pre-filter — active facts on
    /// the page when the page→sub-wiki promotion fired. The trigger is
    /// forma fisica (mass/ramification page→folder), not a single fact's
    /// length (see the [memory model](../../../wiki/concepts/memory-model.md)).
    pub trigger_page_facts: Option<usize>,
    /// Total active facts in the parent wiki — the "weigh against the
    /// parent" signal (see the
    /// [narrative compiler](../../../wiki/design-notes/narrative-compiler.md)):
    /// a page substantial relative to its parent is the one ripe to spin off.
    pub parent_facts: Option<usize>,
    /// Free-form reason string ("REM emergence: page mass 22 of 40 wiki facts").
    pub reason: Option<String>,
}

/// Build the question array stored on a file→sub-wiki receipt.
/// Display-only since the act-first conversion: the dashboard renders
/// it to show what was decided (variant + slug); no approval step reads
/// it back.
fn file_to_subwiki_questions(recommended_slug: &str) -> Value {
    json!([
        {
            "id": "variant",
            "text": "Promote this whole page to its own sub-wiki?",
            "options": [{
                "id": "file_to_subwiki",
                "label": "Promote the page to a new sub-wiki",
                "value": VARIANT_FILE_TO_SUBWIKI,
                "recommended": true,
            }]
        },
        {
            "id": "new_wiki_slug",
            "text": "Slug for the new sub-wiki",
            "options": [{
                "id": "use_recommended",
                "label": format!("Create sub-wiki `{recommended_slug}`"),
                "value": recommended_slug,
                "recommended": true,
            }]
        }
    ])
}

/// Context JSON shared by the `file_to_subwiki` receipt — the same
/// shape the apply handler reads and the dashboard renders.
#[allow(
    clippy::too_many_arguments,
    reason = "emergence carries the new wiki's identity + _meta defaults; a struct would just rename the same fields"
)]
fn file_to_subwiki_context(
    source_wiki_id: &str,
    source_page: &str,
    fact_ids: &[FactId],
    new_wiki_slug: &str,
    style: Option<&str>,
    description: Option<&str>,
    hints: &FileToSubwikiHints,
) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("source_wiki_id".into(), json!(source_wiki_id));
    context.insert("source_page".into(), json!(source_page));
    context.insert(
        "fact_ids".into(),
        json!(fact_ids.iter().map(FactId::as_str).collect::<Vec<_>>()),
    );
    context.insert("variant".into(), json!(VARIANT_FILE_TO_SUBWIKI));
    context.insert("recommended_new_wiki_slug".into(), json!(new_wiki_slug));
    if let Some(s) = style {
        context.insert("new_wiki_style".into(), json!(s));
    }
    if let Some(d) = description {
        context.insert("new_wiki_description".into(), json!(d));
    }
    if let Some(m) = hints.trigger_page_facts {
        context.insert("trigger_page_facts".into(), json!(m));
    }
    if let Some(p) = hints.parent_facts {
        context.insert("parent_facts".into(), json!(p));
    }
    if let Some(r) = &hints.reason {
        context.insert("reason".into(), json!(r));
    }
    Value::Object(context)
}

/// Apply a page→sub-wiki emergence **directly** (act-first).
///
/// Runs the `file_to_subwiki` handler now, then records a
/// **born-applied** `wiki_promote` receipt with an open revert window.
/// No `pending` stage, no approval — the caller (REM) emits the
/// `structure_applied` notice naming the affected user; the dashboard
/// is the undo surface.
///
/// `fact_ids` must be **every** active fact on `source_page` — the
/// handler refuses partial moves.
///
/// `style` is the emerged wiki's **dominant style default** stamped onto
/// its `_meta` (`extra["style"]`) — a hint, not a gate: per-page style
/// still wins when a page deviates (see the
/// [memory model](../../../wiki/concepts/memory-model.md)). `None`
/// (or a value outside the closed palette) means the wiki is **generic**
/// and carries no style default. `description` is the free-text "what
/// goes in here" stamped onto `_meta` (`extra["summary"]`); its wording
/// also encodes how strict the style hint is. Both feed the bidirectional
/// root index (placement + recall navigation).
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler fails (nothing
/// changed); [`DirectPromoteError::Receipt`] when the change applied
/// but the undo receipt could not be written.
#[allow(
    clippy::too_many_arguments,
    reason = "emergence carries the new wiki's identity + _meta defaults; a struct would just rename the same fields"
)]
pub async fn apply_file_to_subwiki_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    source_wiki_id: &str,
    source_page: &str,
    fact_ids: &[FactId],
    new_wiki_slug: &str,
    style: Option<&str>,
    description: Option<&str>,
    hints: &FileToSubwikiHints,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = file_to_subwiki_context(
        source_wiki_id,
        source_page,
        fact_ids,
        new_wiki_slug,
        style,
        description,
        hints,
    );
    let answers = json!({
        "variant": VARIANT_FILE_TO_SUBWIKI,
        "new_wiki_slug": new_wiki_slug,
    });
    let spec = apply_file_to_subwiki(pool, tree, &context, &answers).await?;
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::WIKI_PROMOTE,
            context,
            file_to_subwiki_questions(new_wiki_slug),
        )
        .with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        revert_deadline: receipt.revert_deadline,
        spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
    use crate::embedder::{Embedder, FakeEmbedder};
    use crate::types::Principal;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;
    use tempfile::TempDir;

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

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("fake", 4))
    }

    fn seed_alice(tree: &WikiTree) {
        let dir = tree.wikis_dir().join("alice");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = "---\n\
                    wiki_id: alice\n\
                    wiki_type: wiki-user\n\
                    parent_wiki_id: null\n\
                    slug: alice\n\
                    title: Alice\n\
                    acl_default: 'user:alice'\n\
                    ---\n";
        std::fs::write(dir.join("_meta.md"), meta).unwrap();
    }

    fn seed_wiki(tree: &WikiTree, id: &str) {
        let dir = tree.wikis_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\n\
             wiki_id: {id}\n\
             wiki_type: wiki-user\n\
             parent_wiki_id: null\n\
             slug: {id}\n\
             title: {id}\n\
             acl_default: 'user:{id}'\n\
             ---\n",
        );
        std::fs::write(dir.join("_meta.md"), meta).unwrap();
    }

    async fn capture_in(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        wiki: &str,
        page: &str,
        body: &str,
    ) -> FactId {
        let owner = format!("user:{wiki}");
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: owner.parse::<Principal>().unwrap(),
            allow: vec![],
            sender: None,
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

    async fn capture_one(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        page: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
            allow: vec![],
            sender: None,
            fact_type: None,
            topics: vec![],
            dedup_threshold: Some(1.01), // disable dedup for test determinism
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

    async fn setup() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = make_pool().await;
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        seed_alice(&tree);
        (dir, tree, pool)
    }

    #[tokio::test]
    async fn apply_moves_single_fact_paragraph_to_file() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "index.md", "First fact").await;
        let _f_stay = capture_one(&tree, &pool, emb, "index.md", "Stays in place").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "giardinaggio.md" });
        let spec = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        // Spec shape sanity.
        assert_eq!(spec["variant"], "paragraph_to_file");
        assert_eq!(spec["moved_facts"].as_array().unwrap().len(), 1);
        assert_eq!(spec["target_existed_before"], false);
        assert_eq!(spec["moved_facts"][0]["fact_id"], f1.as_str());

        // Target page exists and contains the marker for f1.
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("giardinaggio.md"))
                .unwrap();
        assert!(target.contains(&format!("f={f1}")));
        assert!(target.contains("First fact"));

        // Source page no longer contains f1's marker, but still has the stayer.
        let source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();
        assert!(!source.contains(&format!("f={f1}")));
        assert!(source.contains("Stays in place"));

        // fact_index row repointed.
        let row = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/giardinaggio.md");
    }

    #[tokio::test]
    async fn page_merge_apply_then_revert_round_trips() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let stay = capture_one(
            &tree,
            &pool,
            emb.clone(),
            "viaggi.md",
            "Trip fact that stays",
        )
        .await;
        let h1 = capture_one(
            &tree,
            &pool,
            emb.clone(),
            "viaggi_parigi.md",
            "Hotel booked",
        )
        .await;
        let h2 = capture_one(
            &tree,
            &pool,
            emb,
            "viaggi_parigi.md",
            "Louvre tickets bought",
        )
        .await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "viaggi_parigi.md",
            "fact_ids": [h1.as_str(), h2.as_str()],
            "husk_title": "Viaggio a Parigi",
            "husk_description": "the paris trip",
            "husk_style": "prosa",
        });
        let ans = json!({ "variant": "page_merge", "target_page": "viaggi.md" });
        let spec = apply_page_merge(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        assert_eq!(spec["variant"], "page_merge");
        assert_eq!(spec["moved_facts"].as_array().unwrap().len(), 2);

        // The husk is gone; the survivor carries every marker.
        let husk_path = tree.wikis_dir().join("alice/viaggi_parigi.md");
        assert!(!husk_path.exists(), "husk deleted");
        let survivor = std::fs::read_to_string(tree.wikis_dir().join("alice/viaggi.md")).unwrap();
        for f in [&stay, &h1, &h2] {
            assert!(survivor.contains(&format!("f={f}")), "marker {f} present");
        }
        for f in [&h1, &h2] {
            let row = fact_index::find_by_id(&pool, f).await.unwrap().unwrap();
            assert_eq!(row.source_path, "wikis/alice/viaggi.md");
            assert!(row.region_start.is_some(), "offsets stamped on survivor");
        }

        // Revert: the husk file is recreated from the stored shell, the
        // survivor sheds the moved markers, rows repoint back.
        revert_page_merge(&pool, &tree, &spec)
            .await
            .expect("revert");
        assert!(husk_path.exists(), "husk recreated");
        let husk = std::fs::read_to_string(&husk_path).unwrap();
        for f in [&h1, &h2] {
            assert!(husk.contains(&format!("f={f}")), "marker {f} restored");
        }
        assert!(husk.contains("Hotel booked") && husk.contains("Louvre tickets bought"));
        let survivor2 = std::fs::read_to_string(tree.wikis_dir().join("alice/viaggi.md")).unwrap();
        assert!(!survivor2.contains(&format!("f={h1}")));
        assert!(!survivor2.contains(&format!("f={h2}")));
        assert!(survivor2.contains(&format!("f={stay}")), "stayer untouched");
        for f in [&h1, &h2] {
            let row = fact_index::find_by_id(&pool, f).await.unwrap().unwrap();
            assert_eq!(row.source_path, "wikis/alice/viaggi_parigi.md");
            assert!(row.region_start.is_some(), "offsets stamped on husk");
        }
    }

    #[tokio::test]
    async fn page_merge_refuses_a_partial_move() {
        // Deleting the husk with an active row still on it would strand the
        // row for the orphan sweep — the handler must refuse.
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let h1 = capture_one(
            &tree,
            &pool,
            emb.clone(),
            "viaggi_parigi.md",
            "Hotel booked",
        )
        .await;
        let _h2 = capture_one(&tree, &pool, emb, "viaggi_parigi.md", "Louvre tickets").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "viaggi_parigi.md",
            "fact_ids": [h1.as_str()],
        });
        let ans = json!({ "variant": "page_merge", "target_page": "viaggi.md" });
        let err = apply_page_merge(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("partial merge must be refused");
        assert!(
            err.to_string().contains("every active fact"),
            "refusal names the completeness guard: {err}"
        );
        // Nothing changed on disk or in the DB.
        assert!(tree.wikis_dir().join("alice/viaggi_parigi.md").exists());
        let row = fact_index::find_by_id(&pool, &h1).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/viaggi_parigi.md");
    }

    #[tokio::test]
    async fn page_merge_never_touches_a_foundation_index() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "index.md", "Identity fact").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "variant": "page_merge", "target_page": "viaggi.md" });
        let err = apply_page_merge(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("index.md must be refused");
        assert!(err.to_string().contains("index.md"), "{err}");
    }

    #[tokio::test]
    async fn apply_moves_multiple_facts_in_order() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "index.md", "Fact A").await;
        let f2 = capture_one(&tree, &pool, emb.clone(), "index.md", "Fact B").await;
        let f3 = capture_one(&tree, &pool, emb, "index.md", "Fact C").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str(), f2.as_str(), f3.as_str()],
        });
        let ans = json!({ "target_page": "moved.md" });
        let spec = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        let moved = spec["moved_facts"].as_array().unwrap();
        assert_eq!(moved.len(), 3);
        // Order preserved.
        assert_eq!(moved[0]["fact_id"], f1.as_str());
        assert_eq!(moved[1]["fact_id"], f2.as_str());
        assert_eq!(moved[2]["fact_id"], f3.as_str());

        // Target now holds all three.
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("moved.md")).unwrap();
        assert!(target.contains("Fact A"));
        assert!(target.contains("Fact B"));
        assert!(target.contains("Fact C"));

        // All three rows repointed.
        for fid in [&f1, &f2, &f3] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.source_path, "wikis/alice/moved.md");
        }
    }

    #[tokio::test]
    async fn apply_appends_to_pre_existing_target_page() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        // Seed a fact already on target page so it pre-exists with content.
        let _f_target = capture_one(&tree, &pool, emb.clone(), "target.md", "Already there").await;
        let f1 = capture_one(&tree, &pool, emb, "index.md", "Will be moved").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "target.md" });
        let spec = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        assert_eq!(spec["target_existed_before"], true);

        // Target page still has the original content + the appended marker.
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("target.md")).unwrap();
        assert!(target.contains("Already there"));
        assert!(target.contains("Will be moved"));
    }

    #[tokio::test]
    async fn apply_rejects_missing_fact_id() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let _f_real = capture_one(&tree, &pool, emb, "index.md", "Real").await;
        // Use a syntactically-valid but unknown fact id.
        let bogus = "018f1234-5678-7abc-9def-0123456789ab";
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [bogus],
        });
        let ans = json!({ "target_page": "elsewhere.md" });
        let err = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("must reject");
        match err {
            ApplyError::HandlerData(msg) => {
                assert!(msg.contains("not in fact_index"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_rejects_same_source_and_target_page() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "index.md", "x").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "index.md" });
        let err = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("must reject");
        assert!(matches!(err, ApplyError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn apply_rejects_empty_fact_ids() {
        let (_dir, tree, pool) = setup().await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [],
        });
        let ans = json!({ "target_page": "target.md" });
        let err = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("must reject");
        match err {
            ApplyError::InvalidPayload(msg) => {
                assert!(msg.contains("fact_ids must not be empty"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_then_revert_round_trips_to_source() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "index.md", "Movable A").await;
        let f2 = capture_one(&tree, &pool, emb, "index.md", "Movable B").await;
        let original_source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "index.md",
            "fact_ids": [f1.as_str(), f2.as_str()],
        });
        let ans = json!({ "target_page": "moved.md" });
        let spec = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

        revert_paragraph_to_file(&pool, &tree, &spec)
            .await
            .expect("revert");

        // Source contents contain the two markers again (order at end is OK).
        let restored_source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();
        assert!(
            restored_source.contains(&format!("f={f1}")),
            "{restored_source}"
        );
        assert!(
            restored_source.contains(&format!("f={f2}")),
            "{restored_source}"
        );

        // Target page now has no markers for f1/f2 (best-effort cleanup; the
        // file can stay on disk, but the markers must be gone).
        let target_after =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("moved.md")).unwrap();
        assert!(!target_after.contains(&format!("f={f1}")), "{target_after}");
        assert!(!target_after.contains(&format!("f={f2}")), "{target_after}");

        // fact_index rows point back at source.
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.source_path, "wikis/alice/index.md");
        }
        // Restored source contains all the original bytes (the markers may
        // be in a different order; just check substring equivalence on the
        // marker presence — the surrounding prose is empty in this fixture).
        for fid in [&f1, &f2] {
            assert!(
                original_source.contains(&format!("f={fid}"))
                    && restored_source.contains(&format!("f={fid}"))
            );
        }
    }

    #[tokio::test]
    async fn revert_rejects_wrong_variant() {
        let (_dir, tree, pool) = setup().await;
        let bogus_spec = json!({"variant": "wiki_type_forge"});
        let err = revert_paragraph_to_file(&pool, &tree, &bogus_spec)
            .await
            .expect_err("must reject");
        assert!(matches!(err, RevertError::InvalidPayload(_)));
    }

    // ---- file → sub-wiki variant ----

    #[tokio::test]
    async fn apply_file_to_subwiki_creates_subwiki_and_moves_facts() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        // Two facts living in alice/giardinaggio.md.
        let f1 = capture_one(&tree, &pool, emb.clone(), "giardinaggio.md", "Note A").await;
        let f2 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note B").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str(), f2.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        assert_eq!(spec["variant"], "file_to_subwiki");
        assert_eq!(spec["new_wiki_id"], "alice-giardinaggio");
        assert_eq!(spec["new_wiki_slug"], "giardinaggio");

        // New sub-wiki directory exists with _meta.md + index.md, source file gone.
        let new_dir = tree.wikis_dir().join("alice").join("giardinaggio");
        assert!(new_dir.exists(), "sub-wiki dir must exist");
        assert!(new_dir.join("_meta.md").exists(), "_meta.md must exist");
        let index = std::fs::read_to_string(new_dir.join("index.md")).unwrap();
        assert!(index.contains("Note A"), "{index}");
        assert!(index.contains("Note B"), "{index}");
        let source_after = tree.wikis_dir().join("alice").join("giardinaggio.md");
        assert!(!source_after.exists(), "source file must be removed");

        // fact_index rows updated: wiki_id = alice-giardinaggio, source_path = new index.
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.wiki_id, "alice-giardinaggio");
            assert_eq!(row.source_path, "wikis/alice/giardinaggio/index.md");
        }
    }

    /// The emergence plan-sync seam: after the apply, the persisted plan
    /// no longer claims the moved facts for the parent page — they live
    /// under the emerged wiki's index entry — and the revert restores the
    /// original shape. Without this, the next carry-over re-rendered the
    /// old page and the compiler's pre-point dragged the rows back.
    #[tokio::test]
    async fn file_to_subwiki_rehomes_the_persisted_plan_and_back() {
        use crate::planner::{
            CompilationPlan, FactForPage, PagePlan, PageType, load_previous_plan, save_plan,
        };
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "giardinaggio.md", "Note A").await;
        let f2 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note B").await;

        // Persist a plan that assigns both facts to the source page.
        let mut facts = Vec::new();
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            facts.push(FactForPage::from_row(&row));
        }
        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "giardinaggio".to_owned(),
            PagePlan {
                slug: "giardinaggio".to_owned(),
                title: "Giardinaggio".to_owned(),
                description: "garden notes".to_owned(),
                style: None,
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: facts,
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "giardinaggio.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            compilation_order: vec!["giardinaggio".to_owned()],
            link_graph: std::collections::BTreeMap::new(),
            dirty_pages: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 2,
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        save_plan(&tree, &plan).expect("save plan");

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str(), f2.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

        let after = load_previous_plan(&tree).expect("load").expect("plan");
        assert!(
            !after.pages.contains_key("giardinaggio"),
            "the old page left the plan"
        );
        let emerged = after
            .pages
            .get("alice_giardinaggio")
            .expect("the emerged wiki's index entered the plan");
        assert_eq!(emerged.wiki_id, "alice-giardinaggio");
        assert_eq!(emerged.page_path, "index.md");
        assert_eq!(emerged.primary_facts.len(), 2, "both facts re-homed");
        assert!(
            after.force_dirty.contains(&"alice_giardinaggio".to_owned()),
            "the emerged index is parked for recompile"
        );

        // The revert restores the original plan shape.
        revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect("revert");
        let back = load_previous_plan(&tree).expect("load").expect("plan");
        assert!(
            !back.pages.contains_key("alice_giardinaggio"),
            "the emerged index left the plan"
        );
        let restored = back
            .pages
            .get("giardinaggio")
            .expect("the source page re-entered the plan");
        assert_eq!(restored.primary_facts.len(), 2, "facts back home");
        assert_eq!(restored.wiki_id, "alice");
    }

    #[tokio::test]
    async fn apply_file_to_subwiki_rejects_existing_target_path() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note").await;
        // Pre-create the sub-wiki dir.
        std::fs::create_dir_all(tree.wikis_dir().join("alice").join("giardinaggio")).unwrap();

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let err = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("must reject");
        assert!(
            matches!(err, ApplyError::InvalidPayload(ref msg) if msg.contains("already exists"))
        );
    }

    #[tokio::test]
    async fn apply_file_to_subwiki_rejects_partial_fact_set() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "giardinaggio.md", "A").await;
        let _f2 = capture_one(&tree, &pool, emb, "giardinaggio.md", "B").await;
        // Only request f1 — file_to_subwiki must refuse partial moves.
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let err = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("must reject");
        match err {
            ApplyError::InvalidPayload(msg) => {
                assert!(msg.contains("every active fact"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_file_to_subwiki_uses_explicit_slug_override() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({
            "variant": "file_to_subwiki",
            "new_wiki_slug": "ortiamici",
            "new_wiki_title": "Orti & Amici",
        });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        assert_eq!(spec["new_wiki_slug"], "ortiamici");
        assert!(tree.wikis_dir().join("alice").join("ortiamici").exists());
        // Title flows through to _meta.md.
        let meta = std::fs::read_to_string(
            tree.wikis_dir()
                .join("alice")
                .join("ortiamici")
                .join("_meta.md"),
        )
        .unwrap();
        assert!(meta.contains("Orti & Amici"), "{meta}");
    }

    #[tokio::test]
    async fn apply_then_revert_file_to_subwiki_round_trips() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "giardinaggio.md", "Bytes A").await;
        let f2 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Bytes B").await;
        let original_source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("giardinaggio.md"))
                .unwrap();

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str(), f2.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

        revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect("revert");

        // Sub-wiki gone; source restored byte-for-byte.
        let new_dir = tree.wikis_dir().join("alice").join("giardinaggio");
        assert!(!new_dir.exists(), "sub-wiki dir must be torn down");
        let source_after =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("giardinaggio.md"))
                .unwrap();
        assert_eq!(
            source_after, original_source,
            "source bytes must round-trip"
        );

        // fact_index rows restored.
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.wiki_id, "alice");
            assert_eq!(row.source_path, "wikis/alice/giardinaggio.md");
        }
    }

    #[tokio::test]
    async fn revert_file_to_subwiki_refuses_when_subwiki_has_extra_file() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

        // Simulate user adding a page to the new sub-wiki between apply and revert.
        std::fs::write(
            tree.wikis_dir()
                .join("alice")
                .join("giardinaggio")
                .join("notes.md"),
            "user edits\n",
        )
        .unwrap();

        let err = revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect_err("must reject");
        match err {
            RevertError::HandlerData(msg) => {
                assert!(msg.contains("extra entries"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
        // Sub-wiki dir + the user's extra file still there.
        assert!(
            tree.wikis_dir()
                .join("alice")
                .join("giardinaggio")
                .join("notes.md")
                .exists()
        );
    }

    #[tokio::test]
    async fn apply_file_to_subwiki_direct_applies_and_records_receipt() {
        // Proves the act-first path: the emergence applies immediately,
        // the born-applied receipt lands in `applied` with an open
        // revert window, and no `pending` row ever exists.
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "giardinaggio.md", "Note A").await;
        let f2 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note B").await;

        let hints = FileToSubwikiHints {
            trigger_page_facts: Some(2),
            parent_facts: Some(2),
            reason: Some("test emergence".into()),
        };
        let receipt = apply_file_to_subwiki_direct(
            &pool,
            &tree,
            "alice",
            "giardinaggio.md",
            &[f1.clone(), f2.clone()],
            "giardinaggio",
            Some("lista"),
            Some("Gardening notes; usually lists, a prose page is OK if it fits"),
            &hints,
            None,
        )
        .await
        .expect("direct apply");

        // The receipt row is born `applied` with an undo token + open
        // window — the undo anchor the dashboard revert path reads.
        let (status, context, token): (String, String, Option<String>) = sqlx::query_as(
            "SELECT status, context, revert_token FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&receipt.proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert!(token.is_some(), "born-applied receipt must carry a token");
        assert!(receipt.revert_deadline > chrono::Utc::now());
        let ctx: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "file_to_subwiki");
        assert_eq!(ctx["source_page"], "giardinaggio.md");
        assert_eq!(ctx["trigger_page_facts"], 2);
        assert_eq!(ctx["parent_facts"], 2);
        assert_eq!(receipt.spec["new_wiki_id"], "alice-giardinaggio");
        let new_dir = tree.wikis_dir().join("alice").join("giardinaggio");
        assert!(new_dir.join("_meta.md").exists(), "sub-wiki must exist");
        let index = std::fs::read_to_string(new_dir.join("index.md")).unwrap();
        assert!(
            index.contains("Note A") && index.contains("Note B"),
            "{index}"
        );
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.wiki_id, "alice-giardinaggio");
        }
        // The emergence-decided _meta defaults are stamped: style
        // default + description, re-read through the parser.
        let emerged = tree
            .locate(&WikiId::parse("alice-giardinaggio").unwrap())
            .expect("locate emerged");
        assert_eq!(
            emerged.meta().extra.get("style").and_then(|v| v.as_str()),
            Some("lista"),
        );
        assert_eq!(
            emerged.meta().extra.get("summary").and_then(|v| v.as_str()),
            Some("Gardening notes; usually lists, a prose page is OK if it fits"),
        );
    }

    #[tokio::test]
    async fn fact_refile_direct_moves_cross_wiki_then_revert_restores() {
        // The act-first cross-wiki refile: a fact captured in alice moves
        // to bob via the direct path (born-applied receipt + open window),
        // and revert puts it back — wiki_id + prose + offsets restored.
        let (_dir, tree, pool) = setup().await;
        seed_wiki(&tree, "bob");
        let emb = embedder();

        let f = capture_in(
            &tree,
            &pool,
            emb.clone(),
            "alice",
            "index.md",
            "Belongs to bob",
        )
        .await;
        let _stay = capture_in(
            &tree,
            &pool,
            emb.clone(),
            "alice",
            "index.md",
            "Stays in alice",
        )
        .await;
        // A pre-existing fact in bob so the dest page already has content.
        let _b = capture_in(&tree, &pool, emb, "bob", "index.md", "Bob's own note").await;

        let receipt = apply_fact_refile_direct(
            &pool,
            &tree,
            &f,
            "alice",
            "index.md",
            "bob",
            "index.md",
            Some("LLM: this fact is about bob"),
            None,
        )
        .await
        .expect("direct refile");

        // Born-applied receipt: status applied, token + open window.
        let (status, context, token): (String, String, Option<String>) = sqlx::query_as(
            "SELECT status, context, revert_token FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&receipt.proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert!(token.is_some(), "born-applied receipt must carry a token");
        assert!(receipt.revert_deadline > chrono::Utc::now());
        let ctx: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "fact_refile");
        assert_eq!(ctx["fact_id"], f.as_str());
        assert_eq!(ctx["recommended_dest_wiki_id"], "bob");

        // The fact_index row repointed to bob.
        let row = fact_index::find_by_id(&pool, &f).await.unwrap().unwrap();
        assert_eq!(row.wiki_id, "bob");
        assert_eq!(row.source_path, "wikis/bob/index.md");

        // Disk: marker gone from alice, present in bob; the stayer + bob's
        // own note untouched.
        let alice_idx =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();
        let bob_idx =
            std::fs::read_to_string(tree.wikis_dir().join("bob").join("index.md")).unwrap();
        assert!(!alice_idx.contains(&format!("f={f}")));
        assert!(alice_idx.contains("Stays in alice"));
        assert!(bob_idx.contains(&format!("f={f}")));
        assert!(bob_idx.contains("Belongs to bob"));
        assert!(bob_idx.contains("Bob's own note"));

        // Revert via the chassis router restores wiki_id + prose.
        revert_wiki_promote(&pool, &tree, &receipt.spec)
            .await
            .expect("revert");
        let row = fact_index::find_by_id(&pool, &f).await.unwrap().unwrap();
        assert_eq!(row.wiki_id, "alice");
        assert_eq!(row.source_path, "wikis/alice/index.md");
        let alice_idx =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();
        let bob_idx =
            std::fs::read_to_string(tree.wikis_dir().join("bob").join("index.md")).unwrap();
        assert!(alice_idx.contains(&format!("f={f}")));
        assert!(alice_idx.contains("Belongs to bob"));
        assert!(!bob_idx.contains(&format!("f={f}")));
        assert!(bob_idx.contains("Bob's own note"));
    }

    #[tokio::test]
    async fn fact_refile_refuses_same_wiki() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f = capture_one(&tree, &pool, emb, "index.md", "x").await;
        let ctx = json!({
            "fact_id": f.as_str(),
            "source_wiki_id": "alice",
            "source_page": "index.md",
        });
        let ans = json!({ "dest_wiki_id": "alice", "dest_page": "other.md" });
        let err = apply_fact_refile(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("same-wiki must be refused");
        assert!(matches!(err, ApplyError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn revert_file_to_subwiki_refuses_when_source_already_recreated() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "giardinaggio.md", "Note").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "giardinaggio.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "variant": "file_to_subwiki" });
        let spec = apply_wiki_promote(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

        // Operator manually re-created the source file.
        std::fs::write(
            tree.wikis_dir().join("alice").join("giardinaggio.md"),
            "manual\n",
        )
        .unwrap();

        let err = revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect_err("must reject");
        match err {
            RevertError::HandlerData(msg) => {
                assert!(msg.contains("already exists"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---------- validity_close ----------

    /// The closure receipt's emit + revert round-trip: ingest closes a
    /// fact, the receipt's spec snapshots the previous window, and the
    /// chassis revert (dispatched on the variant) reopens it exactly.
    #[tokio::test]
    async fn validity_close_emit_then_revert_reopens_the_window() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "watchlist.md", "Vuole vedere Jumanji").await;

        // Stamp a successor too, so the revert's pointer-clearing is covered.
        let successor = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dff").unwrap();
        let prev = fact_index::close_validity(
            &pool,
            &f1,
            "2026-06-10T22:00:00Z",
            fact_index::decay::COMPLETED,
            Some(&successor),
        )
        .await
        .expect("close")
        .expect("active");
        let applied = emit_validity_close_receipt(
            &pool,
            &[AppliedClosure {
                fact_id: f1.clone(),
                wiki_id: "alice".to_owned(),
                preview: "Vuole vedere Jumanji".to_owned(),
                valid_to: "2026-06-10T22:00:00Z".to_owned(),
                reason: fact_index::decay::COMPLETED.to_owned(),
                prev,
                surface: ClosureSurface::Fact,
            }],
            Some("ieri sera abbiamo visto Jumanji"),
            Some("alice"),
            Some("user:alice".to_owned()),
        )
        .await
        .expect("receipt");
        assert_eq!(applied.spec["variant"], VARIANT_VALIDITY_CLOSE);

        revert_wiki_promote(&pool, &tree, &applied.spec)
            .await
            .expect("revert");
        let row = fact_index::find_by_id(&pool, &f1)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_none(), "the window reopened");
        assert!(row.decay_reason.is_none(), "the reason cleared");
        assert!(
            row.successor_fact_id.is_none(),
            "the successor pointer cleared with the closure"
        );
    }

    /// A revert whose target vanished (tombstoned in the meantime) is
    /// skipped softly — the batch never fails on a missing row.
    #[tokio::test]
    async fn validity_close_revert_skips_a_vanished_target() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb, "serra.md", "Vuole costruire una serra").await;
        fact_index::close_validity(
            &pool,
            &f1,
            "2026-06-11T00:00:00Z",
            fact_index::decay::RETRACTED,
            None,
        )
        .await
        .expect("close")
        .expect("active");
        let spec = json!({
            "variant": VARIANT_VALIDITY_CLOSE,
            "closures": [{
                "fact_id": f1.as_str(),
                "valid_to": "2026-06-11T00:00:00Z",
                "reason": "retracted",
                "prev_valid_to": null,
                "prev_decay_reason": null,
            }],
        });
        fact_index::mark_forgotten(&pool, &f1, "user_request")
            .await
            .expect("tombstone");
        revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect("soft revert");
    }
}
