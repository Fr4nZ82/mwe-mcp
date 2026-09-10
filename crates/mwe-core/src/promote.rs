// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-kind apply logic for the `wiki_promote` structure
//! proposal kind.
//!
//! The chassis in [`crate::proposals`] dispatches here when a proposal
//! row carries `kind = "wiki_promote"`. This module is responsible for
//! the concrete filesystem + DB work; the chassis owns the state
//! transitions.
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
//! - **page merge**: move **every** active fact of one concept page (the
//!   husk) onto a near-synonym survivor page of the same wiki, delete the
//!   husk file, and re-home the move in the persisted compilation plan —
//!   the cure front of semantic page consolidation (the sub-job that
//!   nominates a pair is `rem::run_page_merge`).
//!   Selected via `answers.variant = "page_merge"`. The receipt records
//!   the husk from the shell stored in the spec.
//!
//! All variants preserve fact ids verbatim — the same marker on disk
//! and the same row in `fact_index` keep their UUID across the move;
//! what changes is the row's `source_path` (always) and `wiki_id`
//! (only for the variants that move a page to another wiki).
//!
//! ## Cross-link rewriting
//!
//! Cross-link text needs rewriting when a `wiki_promote` ends up changing
//! the parts of the path the wikilink syntax depends on. The
//! paragraph → file variant keeps the wiki id intact, so no
//! cross-link rewriting is required. The page-group variants change it —
//! the typical case is a group of pages founding the wiki `giardinaggio/`
//! at the root — and that would leave every `[[alice/giardinaggio]]`
//! written across the corpus naming an address the page no longer answers
//! to.
//!
//! Every variant that moves a page across a wiki line therefore closes
//! with [`retarget_links_after_move`]: one pass over the corpus swapping
//! the **wiki half** of each such link, everything after the first `/`
//! kept byte-for-byte (so a `.md` suffix and an `|display` alias
//! survive), the byte offsets of each rewritten file repaired straight
//! after. A bare `[[alice]]` names the wiki, not a page, and a page move
//! never touches it. This matters more than tidiness: a page is reachable
//! only by a fact hit, a match on its card, or an inbound link somebody
//! wrote — nothing offers it for sitting in the same folder — so a link
//! left behind strands the page's whole neighbourhood.
//!
//! The wiki's **card** does need something on this seam — it is written
//! prose about what lives here, and the compiler copies it into `_meta` —
//! so both wikis' cards are parked for a rewrite
//! ([`park_wiki_cards_for_recompile`]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::fact_index;
use crate::parser::{self, ParseEvent};
use crate::proposals::{self, ApplyError, EmitParams, ProposalsError, kind};
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
    /// Page path within the wiki (relative, e.g. `lavoro.md`).
    source_page: String,
    /// Facts to move. Order is preserved when assembling the target page.
    fact_ids: Vec<String>,
}

/// Answer fields the chassis loads from `structure_proposals.answers`
/// once the user has confirmed via the dashboard. Which variant is being
/// applied is read from `answers.variant`; this shape is the one
/// `paragraph_to_file` needs, and it is the only variant that reads an
/// answer at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromoteAnswers {
    /// Page path within the source wiki to append the regions to.
    /// Created if it does not exist.
    target_page: String,
}

/// One row in the `spec.moved_facts` array — what the chassis writes
/// to the proposal row's `spec` column so the receipt says what moved.
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

/// `spec` payload written to the proposal row on a successful apply —
/// the receipt's record of what moved where.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromoteSpec {
    /// Variant discriminator. Stable across schema evolution: future
    /// variants add new values, never reuse this string.
    variant: String,
    source_wiki_id: String,
    source_page: String,
    target_page: String,
    moved_facts: Vec<MovedFactRecord>,
}

/// `spec` payload of a successful cross-wiki single-fact refile: the
/// source/dest identities plus the same `(old/new offset)` record the
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
    /// The single moved fact's offset record.
    moved: MovedFactRecord,
}

// The `variant` discriminator carried at the top of every `wiki_promote`
// `context` and `spec`. Public because they are the vocabulary a reader of
// those rows has to match on — the dashboard's proposals page names each
// one in the reader's words — and a second copy of the strings elsewhere
// would drift the first time one of them changed.

/// A run of facts on one page becomes a page of its own in the same wiki.
pub const VARIANT_PARAGRAPH_TO_FILE: &str = "paragraph_to_file";
/// Pages that are one subject area, from ANY wiki, become a wiki of their
/// own at the root.
pub const VARIANT_PAGES_TO_NEW_WIKI: &str = "pages_to_new_wiki";
/// Its twin for a destination that already exists.
pub const VARIANT_PAGES_INTO_WIKI: &str = "pages_into_wiki";
/// Pages leaving their wiki for an unrelated one, page by page.
///
/// Not as a subject area: the page was born in the wrong place, and this is
/// the only gesture that says so about the page as a whole rather than one
/// fact at a time.
pub const VARIANT_PAGES_REHOME: &str = "pages_rehome";
/// One page is folded into another and the emptied one is deleted.
pub const VARIANT_PAGE_MERGE: &str = "page_merge";
/// A single fact moves to another page, possibly in another wiki.
pub const VARIANT_FACT_REFILE: &str = "fact_refile";
/// A batch of facts stopped being true, and their validity was closed.
pub const VARIANT_VALIDITY_CLOSE: &str = "validity_close";
/// A batch of facts kept their meaning and had their dates corrected.
pub const VARIANT_VALIDITY_EDIT: &str = "validity_edit";
/// A batch of facts changed subject or audience — who may read them.
pub const VARIANT_ACL_CHANGE: &str = "acl_change";

/// `wiki_type` label stamped on a topic wiki born out of the grouping pass.
///
/// A bare string label, not a registered type: no gate reads it semantically
/// (the smart/standard gates read the `_meta` smart flag). It is a generic
/// placeholder that keeps `WikiMeta.wiki_type` populated.
const DEFAULT_TOPIC_WIKI_TYPE: &str = "wiki-tech";

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
        VARIANT_PAGES_TO_NEW_WIKI => apply_pages_to_new_wiki(pool, tree, context, answers).await,
        VARIANT_PAGES_INTO_WIKI => apply_pages_into_wiki(pool, tree, context, answers).await,
        VARIANT_PAGES_REHOME => apply_pages_rehome(pool, tree, context, answers).await,
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

    // Append to the target when it is already there under this exact
    // spelling, otherwise coin it — and a coined name is refused when a
    // case-insensitive mirror would collapse it onto a page that exists.
    let existing_target = if wiki::page_exists_byte_exact(handle.abs_dir(), &target_page_path) {
        std::fs::read_to_string(&target_abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {target_rel}: {e}")))?
    } else {
        if let Some(reason) = wiki::page_creation_refusal(handle.abs_dir(), &target_page_path) {
            return Err(ApplyError::HandlerData(format!(
                "answers.target_page {target_rel}: {reason}",
            )));
        }
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
        moved_facts: moved_records,
    };
    Ok(json!(spec))
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

    // As above: append under the exact spelling, or coin a name a mirror
    // will not collapse onto an existing page.
    let existing_target = if wiki::page_exists_byte_exact(dest_handle.abs_dir(), &dest_page_path) {
        std::fs::read_to_string(&dest_abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {dest_rel}: {e}")))?
    } else {
        if let Some(reason) = wiki::page_creation_refusal(dest_handle.abs_dir(), &dest_page_path) {
            return Err(ApplyError::HandlerData(format!(
                "answers.dest_page {dest_rel}: {reason}",
            )));
        }
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

// ---------- page merge variant ----------

/// Plan slug of a wiki-relative page path (`viaggi.md` → `viaggi`, nested
/// paths flatten like the ingest placement); a wiki's **identity card** is the
/// one page keyed per wiki instead, on the wiki's own slug. One mapping,
/// shared with the plan-re-home seed — see
/// [`crate::planner::plan_slug_for_page`].
fn plan_slug_of_page(wiki_id: &str, page: &str) -> String {
    crate::planner::plan_slug_for_page(wiki_id, page)
}

/// Best-effort plan-sync after a move: re-home `moved` facts onto
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
/// core of [`rehome_after_move`] and the emergence seam, where the
/// destination is a page carried into the topic wiki rather than a
/// `<slug>.md` concept leaf. Failures are logged loudly, never returned.
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

/// One page that changed wiki, as the two addresses a wikilink can name it by.
///
/// The move variants carry a page over **under its own name**, so `page` — the
/// wiki-relative path minus `.md`, which is exactly the slug half of the
/// link grammar —
/// is the same on both sides. Only the wiki changes.
#[derive(Debug, Clone)]
struct MovedPageAddress {
    old_wiki_id: String,
    new_wiki_id: String,
    page: String,
    /// The page stem at the NEW address. Equal to `page` for a move that
    /// only changed wiki; different when the page was merged into another.
    new_page: String,
}

impl MovedPageAddress {
    fn stem(page: &std::path::Path) -> String {
        let page = crate::wiki::posix_path(page);
        page.strip_suffix(".md").unwrap_or(&page).to_owned()
    }

    /// A page that changed wiki and kept its name.
    fn new(old_wiki_id: &str, new_wiki_id: &str, page: &std::path::Path) -> Self {
        let page = Self::stem(page);
        Self {
            old_wiki_id: old_wiki_id.to_owned(),
            new_wiki_id: new_wiki_id.to_owned(),
            new_page: page.clone(),
            page,
        }
    }

    /// A page that became **another page** — the merge case, where the old
    /// address stops existing entirely.
    fn renamed(
        old_wiki_id: &str,
        old_page: &std::path::Path,
        new_wiki_id: &str,
        new_page: &std::path::Path,
    ) -> Self {
        Self {
            old_wiki_id: old_wiki_id.to_owned(),
            new_wiki_id: new_wiki_id.to_owned(),
            page: Self::stem(old_page),
            new_page: Self::stem(new_page),
        }
    }
}

/// Rewrite one body's wikilinks that still name a moved page at its **old**
/// address, returning the new body when anything changed.
///
/// When only the wiki changed, only the wiki half of `[[wiki_id/page]]` is
/// swapped and everything after the first `/` is kept byte-for-byte, so a
/// `.md` suffix, an odd spacing or an `|display` alias survives untouched —
/// this repairs an address, it does not restyle a link. When the **page name**
/// changed too (a merge: the old page stops existing), the address has to be
/// rebuilt, and only the `.md` suffix and the `|display` alias are carried
/// across; there is no old spelling left to preserve.
///
/// A **bare** `[[wiki_id]]` names the wiki, not a page, and is never touched
/// by a page move.
fn retarget_wikilinks(body: &str, moves: &[MovedPageAddress]) -> Option<String> {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut pos = 0usize;
    let mut changed = false;
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let inner_start = i + 2;
        let Some(rel) = body[inner_start..].find("]]") else {
            break;
        };
        let inner_end = inner_start + rel;
        let inner = &body[inner_start..inner_end];
        // `|display` is presentation; the address is what precedes it.
        let head = inner.split('|').next().unwrap_or(inner);
        if let Some((wiki, rest)) = head.split_once('/') {
            let rest = rest.trim();
            let page = rest.strip_suffix(".md").unwrap_or(rest);
            let wiki = wiki.trim();
            if let Some(m) = moves
                .iter()
                .find(|m| m.old_wiki_id == wiki && m.page == page)
            {
                out.push_str(&body[pos..inner_start]);
                out.push_str(&m.new_wiki_id);
                if m.new_page == m.page {
                    // Pure address repair: everything after the wiki is the
                    // author's bytes, and they still describe the page.
                    out.push_str(&inner[head.find('/').unwrap_or(0)..]);
                } else {
                    out.push('/');
                    out.push_str(&m.new_page);
                    // `page` is `rest` with the suffix already stripped above,
                    // so the length difference is the suffix.
                    if page.len() != rest.len() {
                        out.push_str(".md");
                    }
                    // `|display` is what the author wanted the reader to see,
                    // and a merge does not change what the sentence meant.
                    if let Some(alias) = inner.get(head.len()..) {
                        out.push_str(alias);
                    }
                }
                pos = inner_end;
                changed = true;
            }
        }
        i = inner_end + 2;
    }
    if !changed {
        return None;
    }
    out.push_str(&body[pos..]);
    Some(out)
}

/// Repoint one file's marker offsets after its bytes moved, so a rewritten
/// link cannot strand the regions below it.
///
/// The same repair `reindex_file` performs, narrowed to one file we just
/// wrote ourselves: re-parse, and for every `{{f=…}}` region push its current
/// offsets onto the row. Best-effort per fact — a row that vanished mid-move
/// is not worth failing an already-applied structural change for.
async fn repoint_markers(pool: &SqlitePool, source_rel: &str, body: &str) {
    for ev in parser::parse(body).events {
        let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
        else {
            continue;
        };
        let Some(fid) = attrs.fact_id else { continue };
        let start = i64::try_from(start).unwrap_or(i64::MAX);
        let end = i64::try_from(end).unwrap_or(i64::MAX);
        if let Err(e) =
            fact_index::move_region(pool, &fid, source_rel, Some(start), Some(end)).await
        {
            tracing::error!(
                fact_id = fid.as_str(),
                source_path = source_rel,
                error = %e,
                "promote: offset repair failed after a link rewrite — reindex will catch it"
            );
        }
    }
}

/// Follow moved pages across the whole corpus: every link that still names one
/// of them at its old wiki is repointed at the new one.
///
/// A page reached its neighbours by the links somebody wrote on them, and a
/// page is reachable by exactly three routes — a fact hit, a match on its
/// card, or an inbound link — so a move that leaves those links behind does
/// not merely make them ugly, it strands the page's whole neighbourhood.
///
/// Rewrites inside a fact's marked region too. That is not a divergence: the
/// bytes in a region are the prose the writing model produced, never a copy
/// of the row's `text`, and the offsets are repaired straight after.
///
/// Smart wikis are skipped — their files belong to the smart consumer.
/// Best-effort and loud, like every seam after an applied move.
async fn retarget_links_after_move(
    pool: &SqlitePool,
    tree: &WikiTree,
    moves: &[MovedPageAddress],
) -> usize {
    if moves.is_empty() {
        return 0;
    }
    let discovered = match tree.walk() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "promote: link retarget could not walk the corpus");
            return 0;
        },
    };
    let mut rewritten = 0usize;
    for d in discovered {
        if d.meta.smart {
            continue;
        }
        let pages = match wiki::list_wiki_pages(&d.abs_dir) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(wiki_id = d.meta.wiki_id.as_str(), error = %e, "promote: link retarget could not list pages");
                continue;
            },
        };
        for p in pages {
            let abs = d.abs_dir.join(&p.rel_path);
            let Ok(body) = std::fs::read_to_string(&abs) else {
                continue;
            };
            let Some(updated) = retarget_wikilinks(&body, moves) else {
                continue;
            };
            let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &abs);
            if let Err(e) = atomic_write(&abs, updated.as_bytes()) {
                tracing::error!(source_path = %source_rel, error = %e, "promote: link retarget write failed");
                continue;
            }
            repoint_markers(pool, &source_rel, &updated).await;
            rewritten += 1;
            tracing::info!(
                source_path = %source_rel,
                "promote: links retargeted after a page changed wiki"
            );
        }
    }
    rewritten
}

/// Context fields for the page-merge variant: the husk's facts plus the
/// identity of both pages (presentation).
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
/// so the receipt can name the page that went away.
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
    /// Husk identity, kept so the receipt names the page that went away.
    husk_title: String,
    husk_description: String,
    husk_style: Option<String>,
    moved_facts: Vec<MovedFactRecord>,
}

/// Apply a `page_merge`: move **every** active fact of the husk page onto
/// the survivor, delete the husk file, and re-home the move in the
/// persisted compilation plan (husk dropped from plan + registry).
///
/// Refuses a partial move: every active `fact_index` row living on the husk
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
    let wiki_id = WikiId::parse(&ctx.source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    // The survivor's wiki: same as the husk's unless a family-scope merge
    // crossed from one wiki to another (older receipts carry None).
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

    let existing_target =
        if wiki::page_exists_byte_exact(target_handle.abs_dir(), &target_page_path) {
            std::fs::read_to_string(&target_abs)
                .map_err(|e| ApplyError::HandlerIo(format!("read {target_rel}: {e}")))?
        } else {
            if let Some(reason) =
                wiki::page_creation_refusal(target_handle.abs_dir(), &target_page_path)
            {
                return Err(ApplyError::HandlerData(format!(
                    "answers.target_page {target_rel}: {reason}",
                )));
            }
            String::new()
        };
    let (new_target, target_offsets) = compose_target(&existing_target, &moved);
    // The husk minus its regions —
    // recreate the deleted file (frontmatter + connective prose preserved).

    // DB rows FIRST (the capture commit-point pattern): repoint every row at
    // the survivor as a pending render so neither the husk deletion nor the
    // survivor write can be misread by the orphan sweep. A family-scope
    // merge that crossed from one wiki to another re-homes the row's
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

    // Every `[[husk]]` written elsewhere now points at a file that does not
    // exist. A page is reachable by exactly three routes (a fact hit, its own
    // card, an authored link), so a dead rail is a third of a page's
    // reachability gone. The rename half of
    // `MovedPageAddress` exists for this call: unlike a move, the husk's
    // address stops existing altogether.
    retarget_links_after_move(
        pool,
        tree,
        &[MovedPageAddress::renamed(
            &ctx.source_wiki_id,
            &source_page_path,
            &target_wiki_str,
            &target_page_path,
        )],
    )
    .await;

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
        husk_title: ctx.husk_title.unwrap_or_default(),
        husk_description: ctx.husk_description.unwrap_or_default(),
        husk_style: ctx.husk_style,
        moved_facts: moved_records,
    };
    Ok(json!(spec))
}

// ---------- page group → wiki variants (regrouping) ----------

/// One page carried by a group move: where it lived, its verbatim bytes
/// at apply time, and the facts that sat on it.
///
/// byte-for-byte. A group move never splits a page — `fact_ids` is
/// **every** active fact on it at apply time, and the apply refuses
/// when the set on disk has since diverged.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupedPage {
    /// Page path relative to the **source** wiki (e.g. `allattamento.md`).
    page: String,
    /// Verbatim file contents at apply time.
    page_bytes: String,
    /// Every active fact that lived on the page.
    fact_ids: Vec<String>,
}

/// `spec` payload for a group of pages that became a wiki, or joined one.
/// Read back by
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupedPagesSpec {
    variant: String,
    source_wiki_id: String,
    new_wiki_id: String,
    new_wiki_slug: String,
    pages: Vec<GroupedPage>,
}

/// `spec` payload for a group of pages that left their wiki for another one.
/// Read back by
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MovedPagesSpec {
    variant: String,
    source_wiki_id: String,
    target_wiki_id: String,
    pages: Vec<GroupedPage>,
}

/// The `context` a page re-home writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupContext {
    /// Wiki the pages currently live in.
    source_wiki_id: String,
    /// Page paths relative to the source wiki, in the order the model
    /// named them.
    pages: Vec<String>,
    /// The wiki that receives the pages.
    #[serde(default)]
    target_wiki_id: Option<String>,
}

/// Context of a `pages_to_new_wiki`: the pages, named across the whole
/// memory, plus the newborn wiki's advisory name.
#[derive(Debug, serde::Deserialize)]
struct NewWikiContext {
    /// `<wiki_id>/<page.md>` for each page, in the order the model named
    /// them. They may come from any number of wikis — that is the point.
    pages: Vec<String>,
    /// Advisory slug for the wiki about to be born (re-derived through
    /// [`crate::slug::derive_slug`]).
    #[serde(default)]
    new_wiki_slug: Option<String>,
    /// Human-readable title.
    #[serde(default)]
    new_wiki_title: Option<String>,
}

/// Context of a `pages_into_wiki`: where the pages go, and which they are.
#[derive(Debug, serde::Deserialize)]
struct IntoWikiContext {
    /// The wiki that receives them. It must already exist.
    target_wiki_id: String,
    /// `<wiki_id>/<page.md>` for each page.
    pages: Vec<String>,
}

/// A page collected for a group move: paths, bytes, and the facts on it,
/// validated against both `fact_index` and the markers on disk.
struct CollectedPage {
    rel_in_wiki: PathBuf,
    abs: PathBuf,
    /// Workdir-relative `source_path` as `fact_index` stores it.
    source_rel: String,
    bytes: String,
    facts: Vec<FactId>,
}

/// Collect + validate every page of a group move.
///
/// Per page: the path is safe
/// (moving a wiki's front page out would decapitate it), the file
/// exists, and the marker set on disk matches the active `fact_index`
/// rows for that `source_path` exactly. A page with no active fact is
/// refused — the grouping pass only ever names pages that carry mass,
/// so an empty one means the caller and the index disagree.
async fn collect_group_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    source_dir: &std::path::Path,
    pages: &[String],
) -> Result<Vec<CollectedPage>, ApplyError> {
    if pages.is_empty() {
        return Err(ApplyError::InvalidPayload(
            "context.pages must not be empty".into(),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        if !seen.insert(page.clone()) {
            return Err(ApplyError::InvalidPayload(format!(
                "context.pages duplicate: {page}",
            )));
        }
        let rel_in_wiki = validated_page_path(page, "context.pages")?;
        let abs = source_dir.join(&rel_in_wiki);
        let source_rel = wiki::workdir_relative_source_path(tree.workdir(), &abs);
        let bytes = std::fs::read_to_string(&abs)
            .map_err(|e| ApplyError::HandlerIo(format!("read {source_rel}: {e}")))?;

        let active = fact_index::find_active_by_source_path(pool, &source_rel)
            .await
            .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
        if active.is_empty() {
            return Err(ApplyError::HandlerData(format!(
                "{source_rel} carries no active fact — refusing to move it as part of a group",
            )));
        }
        let indexed: HashSet<FactId> = active.iter().map(|r| r.fact_id.clone()).collect();
        let on_disk = marker_set(&bytes);
        if on_disk != indexed {
            return Err(ApplyError::HandlerData(format!(
                "{source_rel} marker set diverged from fact_index \
                 (on_disk={on_disk:?}, indexed={indexed:?}) — refusing to move it",
            )));
        }
        // Preserve the index order so the receipt reads like the page.
        let facts = active.iter().map(|r| r.fact_id.clone()).collect();
        out.push(CollectedPage {
            rel_in_wiki,
            abs,
            source_rel,
            bytes,
            facts,
        });
    }
    Ok(out)
}

/// Every `fact_id` marker present in a page's bytes.
fn marker_set(bytes: &str) -> HashSet<FactId> {
    let mut out = HashSet::new();
    for ev in parser::parse(bytes).events {
        if let ParseEvent::Region { attrs, .. } = ev
            && let Some(fid) = attrs.fact_id
        {
            out.insert(fid);
        }
    }
    out
}

/// Write one collected page into `dest_dir` under the same filename,
/// delete it from its old home, and re-point its `fact_index` rows at
/// the new wiki. Byte offsets survive the move because the bytes are
/// copied verbatim.
async fn relocate_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &CollectedPage,
    dest_dir: &std::path::Path,
    dest_wiki_id: &str,
) -> Result<String, ApplyError> {
    let dest_abs = dest_dir.join(&page.rel_in_wiki);
    let dest_rel = wiki::workdir_relative_source_path(tree.workdir(), &dest_abs);
    if wiki::page_exists_byte_exact(dest_dir, &page.rel_in_wiki) {
        return Err(ApplyError::HandlerData(format!(
            "{dest_rel} already exists — refusing to clobber",
        )));
    }
    // A move carries the page's own name rather than coining one, so the
    // shape of the name is not this path's business — but landing beside a
    // sibling that differs only by case is: on a smart consumer's mirror
    // the two become one file, and the pull clobbers whichever arrives
    // second.
    if let Some(reason) = wiki::page_case_conflict(dest_dir, &page.rel_in_wiki) {
        return Err(ApplyError::HandlerData(format!("{dest_rel}: {reason}")));
    }
    atomic_write(&dest_abs, page.bytes.as_bytes())
        .map_err(|e| ApplyError::HandlerIo(format!("atomic_write {dest_rel}: {e}")))?;

    for fid in &page.facts {
        let row = fact_index::find_by_id(pool, fid)
            .await
            .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
            .ok_or_else(|| ApplyError::HandlerData(format!("fact {fid} vanished mid-apply")))?;
        let touched = fact_index::move_to_wiki(
            pool,
            fid,
            dest_wiki_id,
            &dest_rel,
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

    std::fs::remove_file(&page.abs)
        .map_err(|e| ApplyError::HandlerIo(format!("remove {}: {e}", page.source_rel)))?;
    Ok(dest_rel)
}

/// Park the card of a wiki a page just left, so it stops describing what
/// walked out.
///
/// A wiki's one-line description is **written**, by the compiler, when its
/// foundation card page compiles — that card is what
/// `compiler::sync_foundation_summary` copies
/// into `_meta`. So a card that still promises a subject which has moved to
/// another wiki spreads its staleness into one more place. A page leaving does not touch the card,
/// so nothing would have re-derived it: parking the slug on the plan's
/// `force_dirty` makes the next cycle rewrite it against what is actually
/// there.
///
/// The card's plan slug is the wiki's own slug — `@profile.md` is a foundation
/// page, keyed per wiki.
///
/// Takes **every** wiki whose page set changed, not only the one the pages
/// left: a card describes what is in its wiki, so gaining pages dates it
/// exactly as much as losing them. A wiki that is no longer on disk is
/// skipped — parking a card in a wiki that is gone would leave the
/// next build chasing a page that cannot be compiled. Best-effort: a plan that
/// cannot be parked is repaired by the next full rebuild, and the move itself
/// already stands.
fn park_wiki_cards_for_recompile(tree: &WikiTree, wiki_ids: &[&str]) {
    let slugs: Vec<String> = wiki_ids
        .iter()
        .filter(|id| WikiId::parse(id).is_ok_and(|parsed| tree.locate(&parsed).is_ok()))
        .map(|id| crate::planner::plan_slug_for_page(id, wiki::PROFILE_FILENAME))
        .collect();
    if slugs.is_empty() {
        return;
    }
    if let Err(e) = crate::planner::park_force_dirty_in_persisted_plan(tree, &slugs) {
        tracing::error!(
            ?slugs, error = %e,
            "promote: could not park a wiki's card — its description may still name pages that moved"
        );
    }
}

/// The two addresses of every page a group move carried, for the link
/// retarget. Direction is the caller's: an apply passes source→destination,
/// the caller decides which way round they go.
fn moved_addresses<'a>(
    pages: impl IntoIterator<Item = &'a std::path::Path>,
    from_wiki_id: &str,
    to_wiki_id: &str,
) -> Vec<MovedPageAddress> {
    pages
        .into_iter()
        .map(|p| MovedPageAddress::new(from_wiki_id, to_wiki_id, p))
        .collect()
}

/// Follow a moved page in the **concept registry**, the durable record of
/// which wiki a slug belongs to.
///
/// The registry outlives any one plan: the next full rebuild reads it to
/// decide where each page goes, so a page whose files and rows moved while
/// its registry entry stayed behind is put back by that rebuild — it
/// re-points the rows at the old wiki, renders the page there again, and
/// leaves the copy in the new wiki with nothing pointing at it. A reader
/// opening that copy is served regions whose facts live elsewhere, which
/// the per-page ACL map answers by redacting all of them.
///
/// **The entry has to be the moved page's own.** A concept slug is the page
/// STEM and carries no wiki, so `orto.md` in two wikis is one key: rewriting
/// whichever entry the stem finds would drag a namesake in another wiki along
/// with this move, and nothing downstream would say so. The entry is rewritten
/// only when it names the wiki the page is LEAVING; an entry naming a third
/// wiki belongs to that namesake and is left exactly as it is. The moved page
/// then has no registry entry of its own, which is the safe direction — the
/// next rebuild mints one from where its facts now are, and its facts moved.
///
/// Best-effort like its plan sibling, and loud for the same reason: the
/// files and the rows have already moved, so a failure here is a seam to
/// repair, not a reason to undo a move that stands.
fn follow_page_in_registry(
    tree: &WikiTree,
    page_name: &str,
    source_wiki_id: &str,
    dest_wiki_id: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut registry = match crate::planner::load_concept_registry(tree, &now) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "promote: concept registry unreadable — page left pointing at its old wiki");
            return;
        },
    };
    let slug = plan_slug_of_page(dest_wiki_id, page_name);
    let Some(entry) = registry.entries.get_mut(&slug) else {
        return;
    };
    if entry.wiki_id == dest_wiki_id {
        return;
    }
    if entry.wiki_id != source_wiki_id {
        tracing::warn!(
            slug = %slug,
            entry_wiki_id = %entry.wiki_id,
            source_wiki_id,
            dest_wiki_id,
            "promote: the registry entry for this slug belongs to another wiki — left alone"
        );
        return;
    }
    dest_wiki_id.clone_into(&mut entry.wiki_id);
    if let Err(e) = crate::planner::save_concept_registry(tree, &registry) {
        tracing::error!(error = %e, slug = %slug, "promote: concept registry not saved — page left pointing at its old wiki");
    }
}

/// Re-home one moved page in the persisted compilation plan: its facts
/// leave the old page node and land on a page node of the destination
/// wiki. Best-effort, exactly like `paragraph_to_file`'s re-home.
async fn rehome_grouped_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &CollectedPage,
    source_wiki_id: &str,
    dest_wiki_id: &str,
) {
    let page_name = page.rel_in_wiki.to_string_lossy().into_owned();
    let seed = crate::planner::RehomePageSeed::page_in_wiki(&page_name, dest_wiki_id);
    let old_slug = plan_slug_of_page(source_wiki_id, &page_name);
    rehome_rows_with_seed(pool, &page.facts, &seed, &[old_slug], tree).await;
}

/// One page named across the whole memory: the wiki it currently sits in,
/// and its path inside that wiki.
///
/// The grouping reads every shelf at once, so a page it names has to say
/// which shelf it is on — `"franz/salute_padre.md"`. Split once, here.
fn split_qualified_page(qualified: &str) -> Result<(WikiId, String), ApplyError> {
    let (wiki, page) = qualified.split_once('/').ok_or_else(|| {
        ApplyError::InvalidPayload(format!(
            "context.pages entry {qualified} must be `<wiki_id>/<page.md>`",
        ))
    })?;
    let wiki_id = WikiId::parse(wiki)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.pages wiki id {wiki}: {e}")))?;
    if page.is_empty() {
        return Err(ApplyError::InvalidPayload(format!(
            "context.pages entry {qualified} names no page",
        )));
    }
    Ok((wiki_id, page.to_owned()))
}

/// Collect pages named across several wikis, each with the wiki it leaves.
///
/// The single-wiki sibling ([`collect_group_pages`]) takes one directory and
/// page names relative to it; this one takes `<wiki_id>/<page.md>` and locates
/// each wiki itself. Every other check is the same and lives in the sibling —
/// the file exists, it carries live facts, and its markers agree with
/// `fact_index` — so a page is never moved on a stale picture of it.
async fn collect_pages_across_wikis(
    pool: &SqlitePool,
    tree: &WikiTree,
    pages: &[String],
) -> Result<Vec<(String, CollectedPage)>, ApplyError> {
    if pages.is_empty() {
        return Err(ApplyError::InvalidPayload(
            "context.pages must not be empty".into(),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(pages.len());
    for qualified in pages {
        if !seen.insert(qualified.clone()) {
            return Err(ApplyError::InvalidPayload(format!(
                "context.pages duplicate: {qualified}",
            )));
        }
        let (wiki_id, page) = split_qualified_page(qualified)?;
        let handle = tree.locate(&wiki_id).map_err(|e| {
            ApplyError::HandlerData(format!("source wiki {wiki_id} not found: {e}"))
        })?;
        let mut collected =
            collect_group_pages(pool, tree, handle.abs_dir(), std::slice::from_ref(&page)).await?;
        let one = collected
            .pop()
            .ok_or_else(|| ApplyError::HandlerData(format!("{qualified} collected nothing")))?;
        out.push((wiki_id.as_str().to_owned(), one));
    }
    Ok(out)
}

/// Apply a `pages_to_new_wiki` promotion: pages that are one SUBJECT AREA,
/// wherever they currently sit, become a wiki of their own **at the root**.
///
/// The sibling [`collect_group_pages`] takes one wiki's own pages; this
/// one answers a different question. An argument — gardening, a car, a cat, a
/// relative the household looks after — is not inside anybody: its pages are
/// scattered across the shelves where each was first filed, and the wiki that
/// gathers them belongs to no one and hangs under no one (founder,
/// 2026-09-04: *«la wiki nuova nasce nella root»*).
///
/// Nothing about who may read what changes. Each fact carries its own
/// `subject_id` and `allow_ids` across
/// ([`fact_index::move_to_wiki`] writes the wiki and the path and nothing
/// else), so the same people read the same claims before and after — the move
/// rewrites where a page sits, never whose it is.
async fn apply_pages_to_new_wiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    _answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: NewWikiContext = serde_json::from_value(context.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))?;

    let slug_seed = ctx
        .new_wiki_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApplyError::InvalidPayload("context.new_wiki_slug is required".to_owned())
        })?;
    let derived = crate::slug::derive_slug(slug_seed)
        .map_err(|e| ApplyError::InvalidPayload(format!("new_wiki_slug derive: {e}")))?;
    let new_slug = WikiSlug::parse(&derived)
        .map_err(|e| ApplyError::InvalidPayload(format!("new_wiki_slug invalid: {e}")))?;
    // A root wiki's id IS its slug — there is no parent to compose with.
    let new_wiki_id = WikiId::parse(new_slug.as_str())
        .map_err(|e| ApplyError::InvalidPayload(format!("new wiki id: {e}")))?;
    if tree.locate(&new_wiki_id).is_ok() {
        return Err(ApplyError::InvalidPayload(format!(
            "a wiki named {new_wiki_id} already exists",
        )));
    }

    let collected = collect_pages_across_wikis(pool, tree, &ctx.pages).await?;

    let new_title = ctx
        .new_wiki_title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| new_slug.as_str())
        .to_owned();
    let meta = root_wiki_meta(&new_wiki_id, &new_slug, &new_title, context);
    wiki::write_wiki_dir(tree, &meta, /* requires_parent */ false)
        .map_err(|e| ApplyError::HandlerIo(format!("create wiki {new_wiki_id}: {e}")))?;
    let new_wiki_dir = tree
        .locate(&new_wiki_id)
        .map_err(|e| ApplyError::HandlerIo(format!("locate new wiki {new_wiki_id}: {e}")))?
        .abs_dir()
        .to_path_buf();

    let mut spec_pages = Vec::with_capacity(collected.len());
    let mut sources: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (source_wiki_id, page) in &collected {
        relocate_page(pool, tree, page, &new_wiki_dir, new_wiki_id.as_str()).await?;
        rehome_grouped_page(pool, tree, page, source_wiki_id, new_wiki_id.as_str()).await;
        follow_page_in_registry(
            tree,
            &page.rel_in_wiki.to_string_lossy(),
            source_wiki_id,
            new_wiki_id.as_str(),
        );
        retarget_links_after_move(
            pool,
            tree,
            &moved_addresses(
                std::iter::once(page.rel_in_wiki.as_path()),
                source_wiki_id,
                new_wiki_id.as_str(),
            ),
        )
        .await;
        sources.insert(source_wiki_id.clone());
        spec_pages.push(GroupedPage {
            page: page.rel_in_wiki.to_string_lossy().into_owned(),
            page_bytes: page.bytes.clone(),
            fact_ids: page.facts.iter().map(|f| f.as_str().to_owned()).collect(),
        });
    }

    let mut parked: Vec<&str> = sources.iter().map(String::as_str).collect();
    parked.push(new_wiki_id.as_str());
    park_wiki_cards_for_recompile(tree, &parked);

    tracing::info!(
        new_wiki_id = new_wiki_id.as_str(),
        pages = spec_pages.len(),
        from_wikis = sources.len(),
        facts = spec_pages.iter().map(|p| p.fact_ids.len()).sum::<usize>(),
        "promote: pages_to_new_wiki applied",
    );

    Ok(json!(GroupedPagesSpec {
        variant: VARIANT_PAGES_TO_NEW_WIKI.to_owned(),
        source_wiki_id: sources.iter().cloned().collect::<Vec<_>>().join(","),
        new_wiki_id: new_wiki_id.as_str().to_owned(),
        new_wiki_slug: new_slug.as_str().to_owned(),
        pages: spec_pages,
    }))
}

/// The `_meta.md` of a wiki born for an argument.
///
/// **No parent, and that is the whole point.** An argument hangs under
/// nothing: the product paths never read `parent_wiki_id` (neither recall nor
/// capture mentions it), and the one derivation that walks to a root — the
/// scope principal — answers "none" for a wiki that is nobody's, which its
/// callers tolerate. What the wiki carries instead is its `extra`: the
/// grouping's description and dominant style, which are hints about what
/// belongs here, not about who it belongs to.
fn root_wiki_meta(wiki_id: &WikiId, slug: &WikiSlug, title: &str, context: &Value) -> WikiMeta {
    WikiMeta {
        wiki_id: wiki_id.clone(),
        wiki_type: DEFAULT_TOPIC_WIKI_TYPE.to_owned(),
        parent_wiki_id: None,
        slug: slug.clone(),
        title: title.to_owned(),
        scope: None,
        shared_with: Vec::new(),
        style_overrides: serde_yaml::Mapping::new(),
        keywords: serde_yaml::Mapping::new(),
        children: Vec::new(),
        promoted_from: None,
        no_archive: false,
        smart: false,
        is_agent: false,
        created: Some(chrono::Utc::now().to_rfc3339()),
        updated: None,
        extra: newborn_wiki_meta_extra(context),
    }
}

/// The `_meta.extra` a newborn wiki carries: the grouping-decided
/// `summary` (the wiki's scope) and dominant `style` default. Both
/// are hints, not gates — an out-of-palette style leaves the wiki
/// generic.
fn newborn_wiki_meta_extra(context: &Value) -> serde_yaml::Mapping {
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
    extra
}

/// Apply a `pages_rehome` promotion: pages that were born in the wrong wiki
/// move to the one they belong to.
///
/// No page-count floor applies — this is a repair, not an emergence, and one
/// misplaced page is as wrong as nine. The target is any standard wiki other
/// than the source. A wiki is structure, not possession, so nothing here
/// defends a property: what may be read and who answers for it are the fact's
/// own `subject_id` and `allow_ids`, and the move carries both untouched.
async fn apply_pages_rehome(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    _answers: &Value,
) -> Result<Value, ApplyError> {
    move_pages(pool, tree, context).await
}

async fn move_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
) -> Result<Value, ApplyError> {
    let ctx: GroupContext = serde_json::from_value(context.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))?;
    let source_wiki_id = WikiId::parse(&ctx.source_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.source_wiki_id invalid: {e}")))?;
    let target_raw = ctx
        .target_wiki_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApplyError::InvalidPayload("context.target_wiki_id is required".to_owned())
        })?;
    let target_wiki_id = WikiId::parse(target_raw)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.target_wiki_id invalid: {e}")))?;

    let source_handle = tree
        .locate(&source_wiki_id)
        .map_err(|e| ApplyError::HandlerData(format!("source wiki not found: {e}")))?;
    let target_handle = tree
        .locate(&target_wiki_id)
        .map_err(|e| ApplyError::HandlerData(format!("target wiki not found: {e}")))?;
    if target_wiki_id == source_wiki_id {
        return Err(ApplyError::InvalidPayload(format!(
            "{target_wiki_id} is the page's own wiki — a re-home names a different one",
        )));
    }
    if target_handle.meta().smart {
        return Err(ApplyError::InvalidPayload(format!(
            "{target_wiki_id} is a smart wiki — its pages are its consumer's, written verbatim, \
             and the compiler does not own them",
        )));
    }

    let collected = collect_group_pages(pool, tree, source_handle.abs_dir(), &ctx.pages).await?;
    let mut spec_pages = Vec::with_capacity(collected.len());
    for page in &collected {
        relocate_page(
            pool,
            tree,
            page,
            target_handle.abs_dir(),
            target_wiki_id.as_str(),
        )
        .await?;
        rehome_grouped_page(
            pool,
            tree,
            page,
            &ctx.source_wiki_id,
            target_wiki_id.as_str(),
        )
        .await;
        // The plan node alone does not hold a move: the next full rebuild
        // reads the CONCEPT REGISTRY to decide which wiki each slug belongs
        // to, and the compile stage that closes this same nightly cycle is
        // such a rebuild. Without this line the registry keeps naming the wiki
        // the page just left, the compile re-points the rows and re-renders
        // the page there, the orphan sweep deletes the copy in the
        // destination, and the structural review proposes the identical move
        // again the following night — 42 receipts, five pages, not one of them
        // moved.
        follow_page_in_registry(
            tree,
            &page.rel_in_wiki.to_string_lossy(),
            &ctx.source_wiki_id,
            target_wiki_id.as_str(),
        );
        spec_pages.push(GroupedPage {
            page: page.rel_in_wiki.to_string_lossy().into_owned(),
            page_bytes: page.bytes.clone(),
            fact_ids: page.facts.iter().map(|f| f.as_str().to_owned()).collect(),
        });
    }

    retarget_links_after_move(
        pool,
        tree,
        &moved_addresses(
            collected.iter().map(|p| p.rel_in_wiki.as_path()),
            &ctx.source_wiki_id,
            target_wiki_id.as_str(),
        ),
    )
    .await;
    park_wiki_cards_for_recompile(tree, &[&ctx.source_wiki_id, target_wiki_id.as_str()]);

    tracing::info!(
        source_wiki_id = source_wiki_id.as_str(),
        target_wiki_id = target_wiki_id.as_str(),
        pages = spec_pages.len(),
        "promote: page re-home applied",
    );

    Ok(json!(MovedPagesSpec {
        variant: VARIANT_PAGES_REHOME.to_owned(),
        source_wiki_id: ctx.source_wiki_id,
        target_wiki_id: target_wiki_id.as_str().to_owned(),
        pages: spec_pages,
    }))
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
/// move that leaves the fact's bytes intact.
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
/// from "the change IS applied but the receipt could not be
/// recorded" — callers log the second loudly instead of retrying the
/// apply.
#[derive(Debug, thiserror::Error)]
pub enum DirectPromoteError {
    /// The apply handler refused or failed; no receipt was written.
    #[error("{0}")]
    Apply(#[from] ApplyError),
    /// The structural change applied, but inserting the born-applied
    /// receipt failed. The REM WAL op is the remaining audit trail.
    #[error("change applied but receipt failed: {0}")]
    Receipt(#[from] ProposalsError),
}

/// Receipt of a direct (act-first) structural apply: the born-applied
/// row recording what happened, plus the spec the notice event reads.
#[derive(Debug, Clone)]
pub struct DirectApplied {
    /// `structure_proposals` row id of the born-applied receipt — what
    /// the dashboard and the notice event point at.
    pub proposal_id: String,
    /// Spec returned by the apply handler (the `PromoteSpec` /
    /// `GroupedPagesSpec` shape) — carries the concrete target
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
    /// not a single fact's word count.
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
/// **born-applied** `wiki_promote` receipt.
/// There is no `pending` stage and no approval step; the dashboard is the
/// *reading* surface, not an approval surface.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler fails (nothing
/// changed); [`DirectPromoteError::Receipt`] when the change applied
/// but the receipt could not be written.
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
/// `wiki_promote` receipt. No `pending` stage, no approval; the dashboard
/// is where the operator reads what happened, not an approval surface.
/// `reason` is a one-line audit string for the receipt (e.g. the LLM's
/// stated rationale).
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler refuses or fails
/// (nothing changed on disk); [`DirectPromoteError::Receipt`] when the
/// move applied but the receipt could not be written.
#[allow(
    clippy::too_many_arguments,
    reason = "the cross-wiki refile carries both endpoints (fact, source wiki/page, dest wiki/page) + reason + recipient; bundling into a struct would just hide the same fields"
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
        spec,
    })
}

/// Move **one** fact onto a page of a **different** wiki, leaving **no
/// receipt** — the same engine as [`apply_fact_refile_direct`], same commit
/// order, without the `wiki_promote` row.
///
/// The destination page does not have to exist: it is created, holding the
/// moved region.
///
/// A receipt is a record addressed to the operator, and it carries the
/// source wiki and page in clear. The one caller that must not leave one is
/// the erasure of a person ([`crate::gdpr::forget_user`]), where the
/// source wiki *is* the person's id: a receipt there would re-write, into a
/// table the pass does not touch, the very name the pass exists to remove.
///
/// # Errors
///
/// [`ApplyError`] when the refile refuses or fails — nothing changed on
/// disk.
pub async fn refile_fact_across_wikis(
    pool: &SqlitePool,
    tree: &WikiTree,
    fact_id: &FactId,
    source_wiki_id: &str,
    source_page: &str,
    dest_wiki_id: &str,
    dest_page: &str,
) -> Result<(), ApplyError> {
    let context = fact_refile_context(
        fact_id,
        source_wiki_id,
        source_page,
        dest_wiki_id,
        dest_page,
        None,
    );
    let answers = json!({
        "variant": VARIANT_FACT_REFILE,
        "dest_wiki_id": dest_wiki_id,
        "dest_page": dest_page,
    });
    apply_fact_refile(pool, tree, &context, &answers)
        .await
        .map(|_| ())
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
    /// consolidation scope (never an arbitrary wiki pair).
    pub survivor_wiki_id: &'a str,
    /// The husk page (wiki-relative `.md`) — loses all facts, gets deleted.
    pub husk_page: &'a str,
    /// The survivor page (wiki-relative `.md`) — gains the facts.
    pub survivor_page: &'a str,
    /// Every active fact of the husk (the handler refuses partial moves).
    pub fact_ids: &'a [FactId],
    /// Husk identity stored so the receipt names the page that went away.
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
/// No `pending` stage, no approval; the dashboard is where the operator
/// reads what happened.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler fails (nothing changed);
/// [`DirectPromoteError::Receipt`] when the merge applied but the receipt
/// could not be written.
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
        spec,
    })
}

// ---------- The validity_close variant (born-applied closures) ----------

/// One closed target inside a `validity_close` receipt's spec — what was
/// stamped and the window as it was before.
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
/// Nothing downstream needs it (the write probes the fact first, then the
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
    /// Snapshot taken at closure time — what the window was before.
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
/// ⚠️ **Every fact handed in here belongs to the addressee.** The receipt
/// stores a 120-character preview of each one and repeats them in the
/// question label, and nothing re-projects that per reader — so a batch
/// spanning two owners would show each of them the other's text. Callers
/// group first ([`crate::proposals::group_by_recipient`]) and call this
/// once per group.
///
/// The ingest orchestrator has already stamped every target
/// (`fact_index::close_validity` / `capture_buffer::close_validity`);
/// this writes the receipt — the act-first pattern. The ingest caller also
/// notices the affected user, because somebody asked for that closure; a
/// nightly sweep closure notices nobody.
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
        spec,
    })
}

// ---------- The validity_edit variant (born-applied date corrections) ----------

/// One edited target inside a `validity_edit` receipt's spec — the new
/// interval and the interval as it was before.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidityEditRecord {
    fact_id: String,
    /// `valid_from` the edit set (`None` = left unchanged).
    new_valid_from: Option<String>,
    /// `valid_to` the edit set (`None` = left unchanged).
    new_valid_to: Option<String>,
    /// `valid_from` before the edit.
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
    /// Snapshot taken at edit time — what the interval was before.
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
/// ⚠️ **Every fact handed in here belongs to the addressee.** The receipt
/// stores a 120-character preview of each one and repeats them in the
/// question label, and nothing re-projects that per reader — so a batch
/// spanning two owners would show each of them the other's text. Callers
/// group first ([`crate::proposals::group_by_recipient`]) and call this
/// once per group.
///
/// The sibling of [`emit_validity_close_receipt`], for a *correction* of
/// the dates rather than a completion/retraction: the ingest orchestrator
/// has already set every target's interval
/// ([`fact_index::set_validity`] / [`capture_buffer::set_validity`]);
/// this writes the receipt.
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
        spec,
    })
}

// ---------- The acl_change variant (born-applied ACL changes) ----------

/// One changed target inside an `acl_change` receipt's spec — the new ACL,
/// the ACL as it was before, and the audit row it wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AclChangeRecord {
    fact_id: String,
    /// New ACL, principals as wire strings.
    new_subject_id: String,
    new_allow_ids: Vec<String>,
    new_sender_id: Option<String>,
    /// Previous ACL.
    prev_subject_id: String,
    prev_allow_ids: Vec<String>,
    prev_sender_id: Option<String>,
    /// `disclosure_audit.audit_id` the change wrote.
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
    /// New subject.
    pub new_subject: Principal,
    /// New allow-list.
    pub new_allow: Vec<Principal>,
    /// Snapshot taken at change time — what the ACL was before.
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
                        "new_subject_id": c.new_subject.to_string(),
                        "new_allow_ids": principal_strings(&c.new_allow),
                        "prev_subject_id": c.prev.prev_subject_id.to_string(),
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
/// ⚠️ **Every fact handed in here belongs to the addressee.** The receipt
/// stores a 120-character preview of each one and repeats them in the
/// question label, and nothing re-projects that per reader — so a batch
/// spanning two owners would show each of them the other's text. Callers
/// group first ([`crate::proposals::group_by_recipient`]) and call this
/// once per group.
///
/// The sibling of [`emit_validity_close_receipt`], for a sharing change:
/// the ingest orchestrator has already stamped every target's ACL
/// ([`fact_index::set_acl`] / [`capture_buffer::set_acl`]) and written the
/// [`crate::disclosure_audit`] rows; this writes the receipt.
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
                new_subject_id: c.new_subject.to_string(),
                new_allow_ids: principal_strings(&c.new_allow),
                // An acl-change re-shares (subject/allow) only and PRESERVES the
                // fact's cross-user attribution: `set_acl` was called with the
                // prior sender, so the applied sender equals `prev_sender_id`.
                // Record that (not None) so the receipt + disclosure audit
                // match the DB. The receipt records `prev_sender_id` regardless.
                new_sender_id: c.prev.prev_sender_id.as_ref().map(ToString::to_string),
                prev_subject_id: c.prev.prev_subject_id.to_string(),
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
        spec,
    })
}

/// Hints the REM grouping pass attaches to a group receipt for the
/// dashboard to render. Presentation + audit only — no handler reads
/// them.
#[derive(Debug, Clone, Default)]
pub struct PageGroupHints {
    /// Pages in the group — the trigger for a birth is the group's
    /// **size**, never one page's mass.
    pub group_pages: Option<usize>,
    /// Top-level pages the source wiki held when the group was cut.
    pub source_wiki_pages: Option<usize>,
    /// Free-form reason ("rem grouping: 13 of 35 pages of famiglia").
    pub reason: Option<String>,
}

/// Act-first entry point for the whole-memory grouping: pages that are one
/// subject area, from any wikis, become a wiki of their own at the root.
///
/// `pages` are `<wiki_id>/<page.md>`. The floor on how many it takes is the
/// caller's (`auto_promote_group_min_pages`); this wrapper enforces only that
/// the pages exist, carry live facts and move whole.
///
/// # Errors
/// Whatever [`apply_pages_to_new_wiki`] refuses, plus a receipt failure.
#[allow(
    clippy::too_many_arguments,
    reason = "carries the newborn wiki's identity + _meta defaults; a struct would just rename the same fields"
)]
pub async fn apply_pages_to_new_wiki_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    pages: &[String],
    new_wiki_slug: &str,
    new_wiki_title: Option<&str>,
    style: Option<&str>,
    description: Option<&str>,
    hints: &PageGroupHints,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = json!({
        "variant": VARIANT_PAGES_TO_NEW_WIKI,
        "pages": pages,
        "new_wiki_slug": new_wiki_slug,
        "new_wiki_title": new_wiki_title,
        "new_wiki_style": style,
        "new_wiki_description": description,
        "group_pages": hints.group_pages,
        "source_wiki_pages": hints.source_wiki_pages,
        "reason": hints.reason,
    });
    let answers = json!({ "variant": VARIANT_PAGES_TO_NEW_WIKI });
    let spec = apply_pages_to_new_wiki(pool, tree, &context, &answers).await?;
    let questions = json!([{
        "id": "variant",
        "text": format!("Gather {n} pages into a wiki of their own?", n = pages.len()),
        "options": [{
            "id": VARIANT_PAGES_TO_NEW_WIKI,
            "label": format!("Create wiki `{new_wiki_slug}` from {n} pages", n = pages.len()),
            "value": VARIANT_PAGES_TO_NEW_WIKI,
            "recommended": true,
        }]
    }]);
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(kind::WIKI_PROMOTE, context, questions).with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        spec,
    })
}

/// Act-first entry point for the whole-memory grouping's other verb: pages
/// that belong to a wiki which ALREADY EXISTS move into it, from wherever
/// each was filed.
///
/// No floor — the home is already there, so there is nothing to justify.
/// `pages` are `<wiki_id>/<page.md>`, and they may come from any number of
/// wikis including the target's own siblings; a page already in the target is
/// refused rather than moved onto itself.
///
/// # Errors
/// Whatever the handler refuses, plus a receipt failure.
pub async fn apply_pages_into_wiki_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    target_wiki_id: &str,
    pages: &[String],
    hints: &PageGroupHints,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = json!({
        "variant": VARIANT_PAGES_INTO_WIKI,
        "target_wiki_id": target_wiki_id,
        "pages": pages,
        "group_pages": hints.group_pages,
        "reason": hints.reason,
    });
    let answers = json!({ "variant": VARIANT_PAGES_INTO_WIKI });
    let spec = apply_pages_into_wiki(pool, tree, &context, &answers).await?;
    let questions = json!([{
        "id": "variant",
        "text": format!("File {n} pages into `{target_wiki_id}`?", n = pages.len()),
        "options": [{
            "id": VARIANT_PAGES_INTO_WIKI,
            "label": format!("Move {n} pages into `{target_wiki_id}`", n = pages.len()),
            "value": VARIANT_PAGES_INTO_WIKI,
            "recommended": true,
        }]
    }]);
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(kind::WIKI_PROMOTE, context, questions).with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
        spec,
    })
}

/// Apply a `pages_into_wiki`: pages named across the memory move into a wiki
/// that already exists.
///
/// The birth sibling ([`apply_pages_to_new_wiki`]) mints the destination; this
/// one is handed it. Everything else is the same gesture, and the same
/// guarantee: each fact carries its own `subject_id` and `allow_ids` across,
/// so the move rewrites where a page sits and nothing about whose it is.
async fn apply_pages_into_wiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    context: &Value,
    _answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: IntoWikiContext = serde_json::from_value(context.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))?;
    let target_id = WikiId::parse(&ctx.target_wiki_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.target_wiki_id: {e}")))?;
    let target = tree
        .locate(&target_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("target wiki not found: {e}")))?;
    if target.meta().smart {
        return Err(ApplyError::InvalidPayload(format!(
            "{target_id} is smart — its consumer is its only writer",
        )));
    }
    let target_dir = target.abs_dir().to_path_buf();

    let collected = collect_pages_across_wikis(pool, tree, &ctx.pages).await?;
    if let Some((from, _)) = collected
        .iter()
        .find(|(from, _)| from == target_id.as_str())
    {
        return Err(ApplyError::InvalidPayload(format!(
            "a page of {from} cannot move into {target_id} — it is already there",
        )));
    }

    let mut spec_pages = Vec::with_capacity(collected.len());
    let mut sources: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (source_wiki_id, page) in &collected {
        relocate_page(pool, tree, page, &target_dir, target_id.as_str()).await?;
        rehome_grouped_page(pool, tree, page, source_wiki_id, target_id.as_str()).await;
        follow_page_in_registry(
            tree,
            &page.rel_in_wiki.to_string_lossy(),
            source_wiki_id,
            target_id.as_str(),
        );
        retarget_links_after_move(
            pool,
            tree,
            &moved_addresses(
                std::iter::once(page.rel_in_wiki.as_path()),
                source_wiki_id,
                target_id.as_str(),
            ),
        )
        .await;
        sources.insert(source_wiki_id.clone());
        spec_pages.push(GroupedPage {
            page: page.rel_in_wiki.to_string_lossy().into_owned(),
            page_bytes: page.bytes.clone(),
            fact_ids: page.facts.iter().map(|f| f.as_str().to_owned()).collect(),
        });
    }
    let mut parked: Vec<&str> = sources.iter().map(String::as_str).collect();
    parked.push(target_id.as_str());
    park_wiki_cards_for_recompile(tree, &parked);

    tracing::info!(
        target_wiki_id = target_id.as_str(),
        pages = spec_pages.len(),
        from_wikis = sources.len(),
        "promote: pages_into_wiki applied",
    );

    Ok(json!(GroupedPagesSpec {
        variant: VARIANT_PAGES_INTO_WIKI.to_owned(),
        source_wiki_id: sources.iter().cloned().collect::<Vec<_>>().join(","),
        new_wiki_id: target_id.as_str().to_owned(),
        new_wiki_slug: target.meta().slug.as_str().to_owned(),
        pages: spec_pages,
    }))
}

/// Act-first entry point for the structural review: one wiki's pages move to
/// **another wiki**, because that is where they belong.
///
/// The only gesture in the engine that can contradict a page's birth wiki
/// wholesale. Everything
/// else that repairs placement works one fact at a time — which is the wrong
/// grain for the mistake being repaired: a page lands in the wrong wiki as a
/// single decision (a new page joins the wiki of whichever of its facts was
/// listed first), so undoing it fact by fact asks the same question forty
/// times and gets forty independent answers.
///
/// `reason` is the judge's own sentence, kept on the receipt. The move is
/// never undone — the receipt is read, not reverted — so it has to say enough
/// for somebody to tell whether it was right.
///
/// # Errors
///
/// Apply failures surface as [`DirectPromoteError::Apply`]; receipt
/// insertion as [`DirectPromoteError::Proposals`].
pub async fn apply_pages_rehome_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    source_wiki_id: &str,
    target_wiki_id: &str,
    pages: &[String],
    reason: &str,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = json!({
        "variant": VARIANT_PAGES_REHOME,
        "source_wiki_id": source_wiki_id,
        "target_wiki_id": target_wiki_id,
        "pages": pages,
        "reason": reason,
    });
    let answers = json!({ "variant": VARIANT_PAGES_REHOME });
    let spec = apply_pages_rehome(pool, tree, &context, &answers).await?;
    let questions = json!([{
        "id": "variant",
        "text": format!(
            "Move {n} pages from {source_wiki_id} to {target_wiki_id}?",
            n = pages.len()
        ),
        "options": [{
            "id": VARIANT_PAGES_REHOME,
            "label": format!(
                "Move {n} pages from `{source_wiki_id}` to `{target_wiki_id}`",
                n = pages.len()
            ),
            "value": VARIANT_PAGES_REHOME,
            "recommended": true,
        }]
    }]);
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(kind::WIKI_PROMOTE, context, questions).with_recipient(recipient),
        spec.clone(),
        None,
    )
    .await?;
    Ok(DirectApplied {
        proposal_id: receipt.proposal_id,
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
    use std::path::Path;
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
    /// [`capture_one`] into a named wiki, for the tests that need a page to
    /// exist somewhere other than `alice`.
    async fn capture_one_in(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        wiki: &str,
        page: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: format!("user:{wiki}").parse::<Principal>().unwrap(),
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

    /// An argument is not inside anybody, so its wiki is born at the root and
    /// its pages come from wherever each was first filed.
    ///
    /// This is the gesture the per-wiki sibling cannot make: `salute.md` sits
    /// in `alice` and `esami.md` in `bob`, they are one subject, and no shelf
    /// contains both. Nothing about who reads what moves with them — the facts
    /// keep their own `subject_id` and `allow_ids`.
    #[tokio::test]
    async fn pages_from_two_wikis_become_one_wiki_at_the_root() {
        let (_dir, tree, pool) = setup().await;
        seed_wiki(&tree, "bob");
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let emb = embedder();
        capture_one_in(
            &tree,
            &pool,
            emb.clone(),
            "alice",
            "salute.md",
            "note on salute",
        )
        .await;
        capture_one_in(&tree, &pool, emb, "bob", "esami.md", "note on esami").await;

        let applied = apply_pages_to_new_wiki_direct(
            &pool,
            &tree,
            &["alice/salute.md".to_owned(), "bob/esami.md".to_owned()],
            "bilbo",
            Some("Bilbo"),
            Some("prosa-tecnica"),
            Some("Il quadro clinico di Bilbo."),
            &PageGroupHints::default(),
            None,
        )
        .await
        .expect("apply");
        assert_eq!(applied.spec["variant"], "pages_to_new_wiki");

        // The wiki is at the ROOT, with no parent.
        let born = tree.wikis_dir().join("bilbo");
        assert!(
            born.join("_meta.md").exists(),
            "the wiki is born at the root"
        );
        let meta = std::fs::read_to_string(born.join("_meta.md")).unwrap();
        assert!(
            meta.contains("parent_wiki_id: null") || !meta.contains("parent_wiki_id:"),
            "a wiki of an argument hangs under nothing: {meta}"
        );

        // Both pages moved in, and neither is left behind.
        for page in ["salute.md", "esami.md"] {
            assert!(born.join(page).exists(), "{page} moved in");
        }
        assert!(!tree.wikis_dir().join("alice").join("salute.md").exists());
        assert!(!tree.wikis_dir().join("bob").join("esami.md").exists());

        // And the facts followed — a page whose rows stayed behind renders
        // as `[redacted]` to every reader.
        let rows = fact_index::find_active_in_wiki(&pool, "bilbo")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "every fact followed its page");
        // Each kept the subject it arrived with: a move rewrites where a page
        // sits, never whose facts they are.
        let subjects: std::collections::BTreeSet<String> =
            rows.iter().map(|r| r.subject_id.to_string()).collect();
        assert_eq!(
            subjects,
            ["user:alice".to_owned(), "user:bob".to_owned()]
                .into_iter()
                .collect()
        );
    }

    async fn capture_one(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        page: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            slot: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: "user:alice".parse::<Principal>().unwrap(),
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

    /// The two destinations a re-home refuses, and they are the whole of what
    /// separates this verb from regrouping.
    ///
    /// A re-home may name any standard wiki — that is the point of it, and why
    /// it is not the grouping verb — so the only fences left are the two that
    /// are never right: the wiki the page is already in, and a smart wiki,
    /// whose pages belong to its consumer and are written verbatim. Filing
    /// compiler output into one would put marker-wrapped prose inside somebody
    /// else's plain markdown.
    #[tokio::test]
    async fn a_rehome_refuses_the_same_wiki_and_a_smart_one() {
        let (_dir, tree, pool) = setup().await;
        let smart = tree.wikis_dir().join("bobproject");
        std::fs::create_dir_all(&smart).unwrap();
        std::fs::write(
            smart.join("_meta.md"),
            "---\nwiki_id: bobproject\nwiki_type: wiki-user\nparent_wiki_id: null\n\
             slug: bobproject\ntitle: Bob project\nsmart: true\n---\n",
        )
        .unwrap();

        let same = json!({
            "variant": VARIANT_PAGES_REHOME,
            "source_wiki_id": "alice",
            "target_wiki_id": "alice",
            "pages": ["appunti.md"],
        });
        let err = apply_pages_rehome(&pool, &tree, &same, &json!({}))
            .await
            .expect_err("a page cannot move to where it already is");
        assert!(format!("{err}").contains("own wiki"), "unexpected: {err}");

        let into_smart = json!({
            "variant": VARIANT_PAGES_REHOME,
            "source_wiki_id": "alice",
            "target_wiki_id": "bobproject",
            "pages": ["appunti.md"],
        });
        let err = apply_pages_rehome(&pool, &tree, &into_smart, &json!({}))
            .await
            .expect_err("compiler output never lands in a smart wiki");
        assert!(format!("{err}").contains("smart wiki"), "unexpected: {err}");
    }

    /// A re-home carries the page's own name into another wiki, so the
    /// name is not this path's to judge — but the destination is. Landing
    /// `orto.md` beside an existing `Orto.md` makes two pages that a smart
    /// consumer's mirror stores as one file, so the move is refused and the
    /// page stays where it is.
    #[tokio::test]
    async fn a_rehome_refuses_a_destination_that_only_differs_by_case() {
        let (_dir, tree, pool) = setup().await;
        seed_wiki(&tree, "bob");
        capture_one(&tree, &pool, embedder(), "orto.md", "note sull'orto").await;
        let taken = tree.wikis_dir().join("bob").join("Orto.md");
        atomic_write(&taken, b"gia' occupato\n").unwrap();

        let ctx = json!({
            "variant": VARIANT_PAGES_REHOME,
            "source_wiki_id": "alice",
            "target_wiki_id": "bob",
            "pages": ["orto.md"],
        });
        let err = apply_pages_rehome(&pool, &tree, &ctx, &json!({}))
            .await
            .expect_err("a case variant of an existing page is refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("Orto.md"),
            "the refusal names the existing spelling: {msg}"
        );

        // The page did not move and the one already there was not touched.
        // Asked byte-exactly, because where `Orto.md` and `orto.md` are one
        // file `Path::exists` answers about the other spelling and reads a
        // refusal as a completed move.
        assert!(wiki::page_exists_byte_exact(
            &tree.wikis_dir().join("alice"),
            Path::new("orto.md")
        ));
        assert!(!wiki::page_exists_byte_exact(
            &tree.wikis_dir().join("bob"),
            Path::new("orto.md")
        ));
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "gia' occupato\n");
    }

    #[tokio::test]
    async fn apply_moves_single_fact_paragraph_to_file() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "appunti.md", "First fact").await;
        let _f_stay = capture_one(&tree, &pool, emb, "appunti.md", "Stays in place").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "giardinaggio.md" });
        let spec = apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");
        // Spec shape sanity.
        assert_eq!(spec["variant"], "paragraph_to_file");
        assert_eq!(spec["moved_facts"].as_array().unwrap().len(), 1);
        assert_eq!(spec["moved_facts"][0]["fact_id"], f1.as_str());

        // Target page exists and contains the marker for f1.
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("giardinaggio.md"))
                .unwrap();
        assert!(target.contains(&format!("f={f1}")));
        assert!(target.contains("First fact"));

        // Source page no longer contains f1's marker, but still has the stayer.
        let source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("appunti.md")).unwrap();
        assert!(!source.contains(&format!("f={f1}")));
        assert!(source.contains("Stays in place"));

        // fact_index row repointed.
        let row = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/giardinaggio.md");
    }

    /// A move's destination is a name the caller proposes, and a name that
    /// differs from an existing page only by case is TWO pages here and ONE
    /// file on a smart consumer's Windows or macOS mirror — where the next
    /// pull silently clobbers one with the other. The handler refuses and
    /// echoes the spelling already on disk; nothing is written and nothing
    /// moves.
    #[tokio::test]
    async fn paragraph_to_file_refuses_a_target_that_only_differs_by_case() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "appunti.md", "First fact").await;
        // The page the target would collide with, already on disk.
        let _existing = capture_one(&tree, &pool, emb, "ricette.md", "Pasta al pomodoro").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
            "fact_ids": [f1.as_str()],
        });
        let err =
            apply_paragraph_to_file(&pool, &tree, &ctx, &json!({ "target_page": "Ricette.md" }))
                .await
                .expect_err("a case variant of an existing page is refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("ricette.md"),
            "the refusal names the existing spelling: {msg}"
        );

        // Refused means untouched: no second file, the fact still on its
        // page, and the row still pointing at it.
        // Byte-exact: `Path::exists` sees `ricette.md` under the proposed
        // spelling wherever the filesystem folds the two into one file.
        assert!(
            !wiki::page_exists_byte_exact(&tree.wikis_dir().join("alice"), Path::new("Ricette.md")),
            "the colliding page must not have been created"
        );
        let source =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("appunti.md")).unwrap();
        assert!(source.contains(&format!("f={f1}")));
        let row = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/appunti.md");

        // The byte-exact spelling is an append, not a creation, and goes
        // through — the guard refuses a collision, not a lowercase name.
        apply_paragraph_to_file(&pool, &tree, &ctx, &json!({ "target_page": "ricette.md" }))
            .await
            .expect("appending to the existing page is not a creation");
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("ricette.md")).unwrap();
        assert!(target.contains("Pasta al pomodoro"));
        assert!(target.contains("First fact"));
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
    async fn apply_moves_multiple_facts_in_order() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f1 = capture_one(&tree, &pool, emb.clone(), "appunti.md", "Fact A").await;
        let f2 = capture_one(&tree, &pool, emb.clone(), "appunti.md", "Fact B").await;
        let f3 = capture_one(&tree, &pool, emb, "appunti.md", "Fact C").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
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
        let f1 = capture_one(&tree, &pool, emb, "appunti.md", "Will be moved").await;

        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "target.md" });
        apply_paragraph_to_file(&pool, &tree, &ctx, &ans)
            .await
            .expect("apply");

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
        let _f_real = capture_one(&tree, &pool, emb, "appunti.md", "Real").await;
        // Use a syntactically-valid but unknown fact id.
        let bogus = "018f1234-5678-7abc-9def-0123456789ab";
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
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
        let f1 = capture_one(&tree, &pool, emb, "appunti.md", "x").await;
        let ctx = json!({
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
            "fact_ids": [f1.as_str()],
        });
        let ans = json!({ "target_page": "appunti.md" });
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
            "source_page": "appunti.md",
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

    // ---- page group → wiki variants ----

    #[tokio::test]
    async fn fact_refile_refuses_same_wiki() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let f = capture_one(&tree, &pool, emb, "appunti.md", "x").await;
        let ctx = json!({
            "fact_id": f.as_str(),
            "source_wiki_id": "alice",
            "source_page": "appunti.md",
        });
        let ans = json!({ "dest_wiki_id": "alice", "dest_page": "other.md" });
        let err = apply_fact_refile(&pool, &tree, &ctx, &ans)
            .await
            .expect_err("same-wiki must be refused");
        assert!(matches!(err, ApplyError::InvalidPayload(_)));
    }

    // ---------- validity_close ----------

    // ---------- page group → wiki (regrouping) ----------

    /// The address swap, on every shape a link comes in. What must NOT
    /// move is as load-bearing as what must: a bare `[[wiki]]` names a
    /// wiki, not a page, and a page of the same name in another wiki is a
    /// different page.
    #[test]
    fn retarget_wikilinks_swaps_the_wiki_and_leaves_everything_else_alone() {
        let moves = vec![MovedPageAddress::new(
            "alice",
            "alice-giardino",
            std::path::Path::new("orto.md"),
        )];
        let body = "\
Vedi [[alice/orto]] per il resto.
Con alias: [[alice/orto|l'orto]].
Con suffisso: [[alice/orto.md]].
Il wiki intero: [[alice]].
Un'altra wiki: [[bob/orto]].
Un'altra pagina: [[alice/potatura]].
";
        let out = retarget_wikilinks(body, &moves).expect("something changed");
        assert!(out.contains("[[alice-giardino/orto]]"), "{out}");
        assert!(
            out.contains("[[alice-giardino/orto|l'orto]]"),
            "the alias survives: {out}"
        );
        assert!(
            out.contains("[[alice-giardino/orto.md]]"),
            "the suffix survives: {out}"
        );
        assert!(
            out.contains("[[alice]]"),
            "a bare wiki hop is not a page: {out}"
        );
        assert!(
            out.contains("[[bob/orto]]"),
            "another wiki's page is untouched: {out}"
        );
        assert!(
            out.contains("[[alice/potatura]]"),
            "another page is untouched: {out}"
        );
        assert_eq!(out.matches("[[alice-giardino/").count(), 3);

        // Nothing to do ⇒ no rewrite at all, so no file is touched and no
        // offset is disturbed for a page that merely mentions a stranger.
        assert!(retarget_wikilinks("solo [[bob/orto]] qui", &moves).is_none());
    }

    /// A merge renames the page as well as re-addressing it — the husk's
    /// address stops existing, so a link that still names it is a dead rail,
    /// and an authored link is one of only three ways a page is reachable at
    /// all.
    #[test]
    fn retarget_wikilinks_follows_a_page_that_was_merged_into_another() {
        let moves = vec![MovedPageAddress::renamed(
            "bruno",
            std::path::Path::new("salute.md"),
            "famiglia",
            std::path::Path::new("salute_famiglia.md"),
        )];
        let body = "\
Vedi [[bruno/salute]] per il resto.
Con alias: [[bruno/salute|la sua salute]].
Con suffisso: [[bruno/salute.md]].
Un'altra pagina: [[bruno/orto]].
";
        let out = retarget_wikilinks(body, &moves).expect("something changed");
        assert!(out.contains("[[famiglia/salute_famiglia]]"), "{out}");
        assert!(
            out.contains("[[famiglia/salute_famiglia|la sua salute]]"),
            "what the author wanted the reader to see is not the address: {out}"
        );
        assert!(
            out.contains("[[famiglia/salute_famiglia.md]]"),
            "the suffix survives: {out}"
        );
        assert!(
            out.contains("[[bruno/orto]]"),
            "a page that did not move is untouched: {out}"
        );
        assert_eq!(out.matches("bruno/salute").count(), 0);
    }

    /// The whole point, end to end: a page in the wiki left behind still
    /// reaches the page that moved, and the fact markers under the edit
    /// keep pointing at their own bytes.
    #[tokio::test]
    async fn a_page_that_changed_wiki_is_still_reachable_from_the_one_it_left() {
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let neighbour =
            capture_one(&tree, &pool, emb.clone(), "diario.md", "una nota qualsiasi").await;
        // The page has to carry mass to be promotable; which fact does not
        // matter here, because the group variant names pages, not facts.
        capture_one(&tree, &pool, emb, "orto.md", "note sull'orto").await;

        // The neighbour's prose reaches the page that is about to move.
        let diario_abs = tree.wikis_dir().join("alice").join("diario.md");
        let before = std::fs::read_to_string(&diario_abs).unwrap();
        let with_rail = format!("Ne parlo in [[alice/orto]].\n\n{before}");
        atomic_write(&diario_abs, with_rail.as_bytes()).unwrap();
        // The rail sits ABOVE the marked region, so the offsets must shift.
        let before_row = fact_index::find_by_id(&pool, &neighbour)
            .await
            .unwrap()
            .unwrap();

        let ctx = json!({
            "variant": "pages_to_new_wiki",
            "pages": ["alice/orto.md"],
            "new_wiki_slug": "orto",
            "new_wiki_title": "Orto",
        });
        apply_wiki_promote(&pool, &tree, &ctx, &json!({"variant": "pages_to_new_wiki"}))
            .await
            .expect("apply");

        let after = std::fs::read_to_string(&diario_abs).unwrap();
        assert!(
            after.contains("[[orto/orto]]"),
            "the rail follows the page: {after}"
        );
        assert!(
            !after.contains("[[alice/orto]]"),
            "and stops naming the address it left: {after}"
        );

        // The neighbour's own fact still points at its own bytes.
        let after_row = fact_index::find_by_id(&pool, &neighbour)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            before_row.region_start, after_row.region_start,
            "the region moved — otherwise this test proves nothing"
        );
        let start = usize::try_from(after_row.region_start.unwrap()).unwrap();
        let end = usize::try_from(after_row.region_end.unwrap()).unwrap();
        assert!(
            after[start..end].contains(neighbour.as_str()),
            "the repaired offsets frame the marker: {:?}",
            &after[start..end]
        );
    }

    /// A wiki's own card is written prose about what lives in it, and the
    /// compiler copies it into `_meta`. A page leaving touches none of that
    /// by itself, so the move parks the card for a rewrite — on both wikis, since gaining
    /// pages dates a card exactly as much as losing them.
    #[tokio::test]
    async fn a_page_changing_wiki_parks_both_cards_for_a_rewrite() {
        use crate::planner::{CompilationPlan, load_previous_plan, save_plan, slugify};
        let (_dir, tree, pool) = setup().await;
        capture_one(&tree, &pool, embedder(), "orto.md", "note sull'orto").await;
        // A plan must exist for anything to be parked on it.
        let plan = CompilationPlan {
            pages: std::collections::BTreeMap::new(),
            merged_pages: Vec::new(),
            compilation_order: vec!["seed".to_owned()],
            link_graph: std::collections::BTreeMap::new(),
            dirty_pages: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 1,
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        save_plan(&tree, &plan).expect("save plan");

        let ctx = json!({
            "variant": "pages_to_new_wiki",
            "pages": ["alice/orto.md"],
            "new_wiki_slug": "orto",
            "new_wiki_title": "Orto",
        });
        apply_wiki_promote(&pool, &tree, &ctx, &json!({"variant": "pages_to_new_wiki"}))
            .await
            .expect("apply");

        let after = load_previous_plan(&tree).expect("load").expect("plan");
        assert!(
            after.force_dirty.contains(&slugify("alice")),
            "the wiki the page left rewrites its card: {:?}",
            after.force_dirty
        );
        assert!(
            after.force_dirty.contains(&slugify("orto")),
            "and so does the one it joined: {:?}",
            after.force_dirty
        );
    }

    /// The live emergence and the plan. Each carried page keeps its own
    /// plan slug — a slug is the page stem — so the node its old wiki held
    /// must **follow the page into the new wiki**, not be dropped as a
    /// husk. Dropped, the group's facts would have left the plan
    /// altogether, and here that is every page it has: the emptied plan
    /// reads back as no plan at all.
    #[tokio::test]
    async fn a_born_wiki_takes_each_page_node_with_it() {
        use crate::planner::{
            CompilationPlan, FactForPage, PagePlan, load_previous_plan, save_plan,
        };
        let (_dir, tree, pool) = setup().await;
        let emb = embedder();
        let mut pages = std::collections::BTreeMap::new();
        let mut order = Vec::new();
        for page in ["orto.md", "potatura.md"] {
            let fid =
                capture_one(&tree, &pool, emb.clone(), page, &format!("note on {page}")).await;
            let slug = page.trim_end_matches(".md").to_owned();
            let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
            pages.insert(
                slug.clone(),
                PagePlan {
                    slug: slug.clone(),
                    title: slug.clone(),
                    description: String::new(),
                    style: None,
                    primary_facts: vec![FactForPage::from_row(&row)],
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: "alice".to_owned(),
                    page_path: page.to_owned(),
                },
            );
            order.push(slug);
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            compilation_order: order,
            link_graph: std::collections::BTreeMap::new(),
            dirty_pages: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 2,
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        save_plan(&tree, &plan).expect("save plan");

        // The registry entry every real page has: some earlier build wrote it,
        // naming the wiki the page was in at the time.
        {
            let now = "2026-06-08T00:00:00Z";
            let mut reg = crate::planner::load_concept_registry(&tree, now).expect("registry");
            for page in ["orto", "potatura"] {
                reg.entries.insert(
                    page.to_owned(),
                    crate::planner::ConceptRegistryEntry {
                        slug: page.to_owned(),
                        title: page.to_owned(),
                        description: String::new(),
                        style: None,
                        wiki_id: "alice".to_owned(),
                        created_at: now.to_owned(),
                    },
                );
            }
            crate::planner::save_concept_registry(&tree, &reg).expect("seed registry");
        }

        let ctx = json!({
            "variant": "pages_to_new_wiki",
            "pages": ["alice/orto.md", "alice/potatura.md"],
            "new_wiki_slug": "giardino",
            "new_wiki_title": "Giardino",
        });
        apply_wiki_promote(&pool, &tree, &ctx, &json!({"variant": "pages_to_new_wiki"}))
            .await
            .expect("apply");

        let after = load_previous_plan(&tree)
            .expect("load")
            .expect("the plan survives the move");
        for page in ["orto", "potatura"] {
            let node = after
                .pages
                .get(page)
                .unwrap_or_else(|| panic!("{page} kept its plan node"));
            assert_eq!(node.wiki_id, "giardino", "{page} followed the move");
            assert_eq!(node.page_path, format!("{page}.md"));
            assert_eq!(node.primary_facts.len(), 1, "{page} kept its fact");
        }

        // The registry follows too, and it is the record that matters most:
        // it outlives any one plan, and the next FULL rebuild reads it to
        // decide which wiki a page belongs to. An entry left naming the wiki
        // the page came from is a promotion that rebuild quietly undoes — the rows go back, the
        // page is rendered at the old address again, and the copy in the new
        // wiki is left with nothing pointing at it, which a reader is served
        // as a page of `[redacted]`.
        let reg = crate::planner::load_concept_registry(&tree, "2026-06-08T00:00:00Z")
            .expect("registry loads");
        for page in ["orto", "potatura"] {
            assert_eq!(
                reg.entries
                    .get(page)
                    .unwrap_or_else(|| panic!("{page} kept its registry entry"))
                    .wiki_id,
                "giardino",
                "{page}'s registry entry still names the wiki it left",
            );
        }
    }

    /// A re-home is a move, and a move that the next compile undoes is not
    /// one. The registry has to follow the page, exactly as it does when a
    /// group of pages becomes a wiki of its own.
    ///
    /// The concept registry outlives any one plan and the next FULL rebuild
    /// reads it to decide which wiki a slug belongs to — and the compile stage
    /// that closes the same nightly cycle IS such a rebuild. Left naming the
    /// wiki the page came from, it re-points the rows, renders the page at the
    /// old address again and lets the orphan sweep delete the copy at the new
    /// one; the structural review then finds the page still misplaced and
    /// proposes the identical move the following night. Over one replay that
    /// was 42 applied receipts across five pages, one of them proposed
    /// sixteen times, and not a single page anywhere but where it started.
    #[tokio::test]
    async fn a_rehomed_page_takes_its_registry_entry_with_it() {
        let (_dir, tree, pool) = setup().await;
        seed_wiki(&tree, "bob");
        capture_one(&tree, &pool, embedder(), "orto.md", "note sull'orto").await;

        // The registry entry every real page has: an earlier build wrote it,
        // naming the wiki the page was in at the time.
        let now = "2026-06-08T00:00:00Z";
        {
            let mut reg = crate::planner::load_concept_registry(&tree, now).expect("registry");
            reg.entries.insert(
                "orto".to_owned(),
                crate::planner::ConceptRegistryEntry {
                    slug: "orto".to_owned(),
                    title: "Orto".to_owned(),
                    description: String::new(),
                    style: None,
                    wiki_id: "alice".to_owned(),
                    created_at: now.to_owned(),
                },
            );
            crate::planner::save_concept_registry(&tree, &reg).expect("seed registry");
        }

        let ctx = json!({
            "variant": VARIANT_PAGES_REHOME,
            "source_wiki_id": "alice",
            "target_wiki_id": "bob",
            "pages": ["orto.md"],
        });
        apply_pages_rehome(
            &pool,
            &tree,
            &ctx,
            &json!({"variant": VARIANT_PAGES_REHOME}),
        )
        .await
        .expect("apply");

        let reg = crate::planner::load_concept_registry(&tree, now).expect("registry loads");
        assert_eq!(
            reg.entries
                .get("orto")
                .expect("orto kept its registry entry")
                .wiki_id,
            "bob",
            "the registry still names the wiki the page left, so the next compile puts it back",
        );
    }

    /// A page with the same name in a THIRD wiki is not dragged along by
    /// somebody else's move.
    ///
    /// A concept slug is the page stem and carries no wiki, so `orto.md` in
    /// two wikis is one registry key. Following the moved page by that key
    /// alone would repoint a namesake nobody touched — its files and its rows
    /// stay where they are while the registry starts naming another wiki, and
    /// the next rebuild renders it there against facts that are not in it. The
    /// entry is only followed when it names the wiki the page is leaving.
    #[tokio::test]
    async fn a_rehome_leaves_a_namesake_in_another_wiki_alone() {
        let (_dir, tree, pool) = setup().await;
        seed_wiki(&tree, "bob");
        seed_wiki(&tree, "carol");
        capture_one(&tree, &pool, embedder(), "orto.md", "note sull'orto").await;

        // The registry knows one `orto`, and it is CAROL's.
        let now = "2026-06-08T00:00:00Z";
        {
            let mut reg = crate::planner::load_concept_registry(&tree, now).expect("registry");
            reg.entries.insert(
                "orto".to_owned(),
                crate::planner::ConceptRegistryEntry {
                    slug: "orto".to_owned(),
                    title: "Orto".to_owned(),
                    description: String::new(),
                    style: None,
                    wiki_id: "carol".to_owned(),
                    created_at: now.to_owned(),
                },
            );
            crate::planner::save_concept_registry(&tree, &reg).expect("seed registry");
        }

        let ctx = json!({
            "variant": VARIANT_PAGES_REHOME,
            "source_wiki_id": "alice",
            "target_wiki_id": "bob",
            "pages": ["orto.md"],
        });
        apply_pages_rehome(
            &pool,
            &tree,
            &ctx,
            &json!({"variant": VARIANT_PAGES_REHOME}),
        )
        .await
        .expect("apply");

        let reg = crate::planner::load_concept_registry(&tree, now).expect("registry loads");
        assert_eq!(
            reg.entries.get("orto").expect("carol keeps hers").wiki_id,
            "carol",
            "a move out of alice must not repoint carol's page of the same name",
        );
    }
}
