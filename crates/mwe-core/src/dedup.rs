// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-kind apply / revert logic for the `dedup_merge` structure
//! proposal kind.
//!
//! The chassis in [`crate::proposals`] dispatches here when a proposal
//! row carries `kind = "dedup_merge"`. The kind handler itself touches
//! **no filesystem state** — the supersede tombstone lives entirely in
//! the `fact_index` table. The disk half (excising the loser's on-disk
//! region) belongs to the **direct** apply path
//! ([`apply_dedup_merge_direct`], which has the engine context), while a
//! merge applied through the chassis (manual apply of a pending
//! proposal, the overdue auto-apply) leaves the bytes for the
//! light-dream hygiene sweep
//! ([`crate::reindex::sweep_retired_regions`]); the residue redacts
//! fail-closed meanwhile (active ACL map).
//!
//! ## Scope
//!
//! The first variant supported is the canonical "two facts, one
//! survivor" merge. The proposal context names the loser and the
//! winner; the user just confirms by clicking Apply (no answers
//! beyond the implicit confirmation). A future variant could let the
//! user swap loser ↔ winner from the dashboard if the REM picked the
//! wrong survivor, but that ergonomic is deferred — for now the
//! emitter is trusted to pre-select the right direction (the canonical
//! convention is "newer survives").
//!
//! ## Two emitters, one authority model
//!
//! The REM revisor applies **act-first** via
//! [`apply_dedup_merge_direct`]: supersede now, born-applied receipt,
//! `structure_applied` notice, dashboard revert — the same model as
//! every other LLM-confirmed structural move. [`emit_dedup_merge`] (the
//! `pending` + 24h auto-apply lifecycle) remains for emitters that
//! genuinely want a review gate.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::fact_index;
use crate::promote::{DirectApplied, DirectPromoteError};
use crate::proposals::{self, ApplyError, EmitParams, ProposalsError, RevertError, kind};
use crate::types::FactId;
use crate::wiki::WikiTree;

// ---------- Request shapes ----------

/// Context fields loaded from `structure_proposals.context` for a
/// `dedup_merge` proposal. Emitter responsibility (the REM
/// Conciliatore) to populate before insertion. The handler ignores
/// any other field — `similarity`, `loser_summary` etc. are pure
/// presentation hints for the dashboard questionnaire.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DedupContext {
    /// Fact that should become superseded.
    loser_fact_id: String,
    /// Fact that survives as the canonical row.
    winner_fact_id: String,
}

/// `spec` payload written to the proposal row on a successful apply.
/// Read back by [`revert_dedup_merge`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DedupSpec {
    /// Variant discriminator. Stable across schema evolution.
    variant: String,
    loser_fact_id: String,
    winner_fact_id: String,
}

const VARIANT_TWO_WAY_MERGE: &str = "two_way_merge";

// ---------- Apply ----------

/// Apply a `dedup_merge` proposal: mark the loser fact as superseded
/// by the winner.
///
/// Steps:
///
/// 1. Parse + validate `context` — both fact ids must be syntactically
///    valid and must differ.
/// 2. Validate the winner row is active (not superseded, not
///    tombstoned). Loser must be active too — refusing to apply when
///    the loser is already in a supersede chain keeps the revert
///    semantics clean (the inverse can only re-activate what we put
///    down).
/// 3. Call [`fact_index::mark_superseded`]. The marker on disk for the
///    loser fact stays put; the row tombstone in the index is enough
///    to keep it out of recall / search.
/// 4. Return a [`DedupSpec`] for the chassis to stamp on the row.
///
/// The `tree` parameter is accepted for signature uniformity with the
/// other kind handlers; this variant does not touch the filesystem.
///
/// # Errors
///
/// All failure modes funnel into [`ApplyError`].
pub(crate) async fn apply_dedup_merge(
    pool: &SqlitePool,
    _tree: &WikiTree,
    context: &Value,
    _answers: &Value,
) -> Result<Value, ApplyError> {
    let ctx: DedupContext = serde_json::from_value(context.clone())
        .map_err(|e| ApplyError::InvalidPayload(format!("context: {e}")))?;
    let loser = FactId::parse(&ctx.loser_fact_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.loser_fact_id invalid: {e}")))?;
    let winner = FactId::parse(&ctx.winner_fact_id)
        .map_err(|e| ApplyError::InvalidPayload(format!("context.winner_fact_id invalid: {e}")))?;
    if loser == winner {
        return Err(ApplyError::InvalidPayload(
            "context.loser_fact_id and winner_fact_id must differ".into(),
        ));
    }

    let winner_row = fact_index::find_by_id(pool, &winner)
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
        .ok_or_else(|| ApplyError::HandlerData(format!("winner {winner} not in fact_index")))?;
    if winner_row.superseded_at.is_some() || winner_row.deleted_at.is_some() {
        return Err(ApplyError::HandlerData(format!(
            "winner {winner} is superseded or tombstoned",
        )));
    }

    let loser_row = fact_index::find_by_id(pool, &loser)
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?
        .ok_or_else(|| ApplyError::HandlerData(format!("loser {loser} not in fact_index")))?;
    if loser_row.deleted_at.is_some() {
        return Err(ApplyError::HandlerData(format!(
            "loser {loser} is tombstoned",
        )));
    }
    if loser_row.superseded_at.is_some() {
        return Err(ApplyError::HandlerData(format!(
            "loser {loser} is already superseded — apply manually if you want to flip the chain",
        )));
    }

    let touched = fact_index::mark_superseded(pool, &loser, &winner)
        .await
        .map_err(|e| ApplyError::HandlerIo(e.to_string()))?;
    if touched == 0 {
        return Err(ApplyError::HandlerData(format!(
            "mark_superseded updated 0 rows for loser {loser} (concurrent writer?)",
        )));
    }

    tracing::info!(
        loser = %loser,
        winner = %winner,
        "dedup_merge: applied",
    );

    let spec = DedupSpec {
        variant: VARIANT_TWO_WAY_MERGE.to_owned(),
        loser_fact_id: loser.as_str().to_owned(),
        winner_fact_id: winner.as_str().to_owned(),
    };
    Ok(json!(spec))
}

// ---------- Revert ----------

/// Revert a previously-applied `dedup_merge`: clear the supersede
/// tombstone on the loser row so it becomes active again.
///
/// Refuses cleanly if the supersede chain has grown past our pair
/// (`old → new → newer`): the conditional clear matches zero rows and
/// we surface [`RevertError::HandlerData`]. The operator can resolve
/// the chain manually in that rare case.
///
/// # Errors
///
/// All failure modes funnel into [`RevertError`].
pub(crate) async fn revert_dedup_merge(
    pool: &SqlitePool,
    _tree: &WikiTree,
    spec: &Value,
) -> Result<(), RevertError> {
    let spec: DedupSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a DedupSpec: {e}")))?;
    if spec.variant != VARIANT_TWO_WAY_MERGE {
        return Err(RevertError::InvalidPayload(format!(
            "spec.variant {} is not {VARIANT_TWO_WAY_MERGE}",
            spec.variant,
        )));
    }
    let loser = FactId::parse(&spec.loser_fact_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.loser_fact_id invalid: {e}")))?;
    let winner = FactId::parse(&spec.winner_fact_id)
        .map_err(|e| RevertError::InvalidPayload(format!("spec.winner_fact_id invalid: {e}")))?;

    let touched = fact_index::clear_supersede(pool, &loser, &winner)
        .await
        .map_err(|e| RevertError::HandlerIo(e.to_string()))?;
    if touched == 0 {
        return Err(RevertError::HandlerData(format!(
            "loser {loser} is no longer superseded by {winner} — chain has moved on (or row was tombstoned). Resolve manually if you want to revert.",
        )));
    }

    tracing::info!(
        loser = %loser,
        winner = %winner,
        "dedup_merge: reverted",
    );
    Ok(())
}

// ---------- Emit ----------

/// Build the canonical question array for a `dedup_merge` proposal.
/// Single yes/no with "merge" as the recommended branch — the auto-apply
/// path on `timeout_at` falls back to that.
fn dedup_merge_questions() -> Value {
    json!([{
        "id": "confirm",
        "text": "Merge these two near-duplicate facts?",
        "options": [
            {"id": "merge", "label": "Merge", "recommended": true},
        ]
    }])
}

/// Metadata for a `dedup_merge` proposal.
///
/// The REM Conciliatore (and any other emitter) attaches these hints
/// for dashboard presentation. The handler ignores everything except
/// `loser_fact_id` + `winner_fact_id`, but the dashboard surfaces the
/// rest to help the operator decide.
#[derive(Debug, Clone, Default)]
pub struct DedupMergeHints {
    /// Jaccard-6gram score that triggered the proposal. `None` for
    /// emitters that don't compute one (chat-side, manual).
    pub jaccard: Option<f32>,
    /// `wiki_id` both facts live in. Useful when the operator wants
    /// to navigate to the page without first opening the proposal.
    pub source_wiki_id: Option<String>,
    /// Reason string for audit / dashboard tooltip.
    pub reason: Option<String>,
}

/// Emit a pending `dedup_merge` structure proposal. The handler in this
/// module applies it; the chassis manages the lifecycle.
///
/// `winner` is the survivor — the row that remains active. Convention
/// from the REM Conciliatore is "newer wins", but emitters are free to
/// pick any orientation.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for SQL failures.
/// - [`ProposalsError::Json`] for serialisation failures (would only
///   happen on malformed `hints` payload).
pub async fn emit_dedup_merge(
    pool: &SqlitePool,
    winner: &FactId,
    loser: &FactId,
    hints: &DedupMergeHints,
    recipient: Option<String>,
) -> Result<String, ProposalsError> {
    proposals::emit_proposal(
        pool,
        EmitParams::new(
            kind::DEDUP_MERGE,
            dedup_context(winner, loser, hints),
            dedup_merge_questions(),
        )
        .with_recipient(recipient),
    )
    .await
}

/// The `structure_proposals.context` payload both emitters share.
fn dedup_context(winner: &FactId, loser: &FactId, hints: &DedupMergeHints) -> Value {
    let mut context = serde_json::Map::new();
    context.insert("loser_fact_id".into(), json!(loser.as_str()));
    context.insert("winner_fact_id".into(), json!(winner.as_str()));
    if let Some(s) = hints.jaccard {
        context.insert("jaccard".into(), json!(s));
    }
    if let Some(w) = &hints.source_wiki_id {
        context.insert("source_wiki_id".into(), json!(w));
    }
    if let Some(r) = &hints.reason {
        context.insert("reason".into(), json!(r));
    }
    Value::Object(context)
}

/// Apply a two-way merge **directly** (act-first).
///
/// Runs the `dedup_merge` handler now — the loser is superseded by the
/// winner — then records a **born-applied** receipt with an open revert
/// window: the same authority model as auto-promote, the REM page merge,
/// and the ingest validity closures. No `pending` stage, no 24h
/// auto-apply fuse; the caller emits the `structure_applied` notice and
/// the dashboard is the undo surface, not an approval gate.
///
/// The direct path also owns the **disk half**: after the supersede
/// lands, the loser's on-disk region is excised
/// ([`crate::reindex::strip_fact_region`], best-effort — a failure never
/// fails the merge; residue redacts fail-closed and the light-dream
/// hygiene sweep picks it up). A later revert reactivates the loser as a
/// clean pending render (NULL offsets): plan pages re-render it at the
/// next compile; on `rules.md` the behaviour-rules channel keeps serving
/// it from the DB — only the page prose waits.
///
/// # Errors
///
/// [`DirectPromoteError::Apply`] when the handler refuses (nothing
/// changed); [`DirectPromoteError::Receipt`] when the merge applied but
/// the undo receipt could not be written.
pub async fn apply_dedup_merge_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    winner: &FactId,
    loser: &FactId,
    hints: &DedupMergeHints,
    recipient: Option<String>,
) -> Result<DirectApplied, DirectPromoteError> {
    let context = dedup_context(winner, loser, hints);
    let spec = apply_dedup_merge(pool, tree, &context, &json!({})).await?;
    // Disk half of the merge: excise the superseded loser's bytes so the
    // raw page stops carrying the duplicate. Best-effort by design.
    if let Err(e) = crate::reindex::strip_fact_region(pool, tree, embedder, loser).await {
        tracing::warn!(
            loser = %loser,
            error = %e,
            "dedup_merge: loser page-strip failed (redaction still applies)"
        );
    }
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(kind::DEDUP_MERGE, context, dedup_merge_questions())
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
    use crate::types::{Principal, WikiId};
    use crate::wiki::WikiTree;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::path::PathBuf;
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

    async fn setup() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = make_pool().await;
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        seed_alice(&tree);
        (dir, tree, pool)
    }

    async fn capture_one(tree: &WikiTree, pool: &SqlitePool, body: &str) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
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
        let outcome = wiki_capture(tree, pool, embedder(), req).await.unwrap();
        match outcome.action {
            CaptureAction::Captured { .. } => outcome.fact_id,
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_marks_loser_superseded() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice pesa 62 kg").await;
        let winner = capture_one(&tree, &pool, "Alice ora pesa 62 kg").await;

        let ctx = json!({
            "loser_fact_id": loser.as_str(),
            "winner_fact_id": winner.as_str(),
        });
        let spec = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect("apply");
        assert_eq!(spec["variant"], "two_way_merge");
        assert_eq!(spec["loser_fact_id"], loser.as_str());
        assert_eq!(spec["winner_fact_id"], winner.as_str());

        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_some());
        assert_eq!(
            row.superseded_by.as_ref().map(FactId::as_str),
            Some(winner.as_str()),
        );
    }

    #[tokio::test]
    async fn apply_rejects_missing_loser() {
        let (_dir, tree, pool) = setup().await;
        let winner = capture_one(&tree, &pool, "Real fact").await;
        let bogus = "018f1234-5678-7abc-9def-0123456789ab";
        let ctx = json!({
            "loser_fact_id": bogus,
            "winner_fact_id": winner.as_str(),
        });
        let err = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect_err("must reject");
        match err {
            ApplyError::HandlerData(msg) => {
                assert!(
                    msg.contains("loser") && msg.contains("not in fact_index"),
                    "{msg}"
                );
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_rejects_self_merge() {
        let (_dir, tree, pool) = setup().await;
        let f = capture_one(&tree, &pool, "Same").await;
        let ctx = json!({
            "loser_fact_id": f.as_str(),
            "winner_fact_id": f.as_str(),
        });
        let err = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect_err("must reject");
        assert!(matches!(err, ApplyError::InvalidPayload(_)), "{err:?}");
    }

    #[tokio::test]
    async fn apply_rejects_already_superseded_loser() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Old").await;
        let winner1 = capture_one(&tree, &pool, "Mid").await;
        let winner2 = capture_one(&tree, &pool, "New").await;
        // Pre-supersede loser by winner1.
        fact_index::mark_superseded(&pool, &loser, &winner1)
            .await
            .unwrap();
        let ctx = json!({
            "loser_fact_id": loser.as_str(),
            "winner_fact_id": winner2.as_str(),
        });
        let err = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect_err("must reject");
        match err {
            ApplyError::HandlerData(msg) => {
                assert!(msg.contains("already superseded"), "{msg}");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_rejects_superseded_winner() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "A").await;
        let winner = capture_one(&tree, &pool, "B").await;
        let _newer = capture_one(&tree, &pool, "C").await;
        // Mark winner as already superseded (so applying a merge into it is invalid).
        fact_index::mark_superseded(&pool, &winner, &loser)
            .await
            .unwrap();
        // Now flip the chain so the test request asks to merge into winner (which is superseded).
        let ctx = json!({
            "loser_fact_id": loser.as_str(),
            "winner_fact_id": winner.as_str(),
        });
        let err = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect_err("must reject");
        match err {
            ApplyError::HandlerData(msg) => assert!(msg.contains("winner"), "{msg}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Act-first: the merge lands immediately, the receipt is born
    /// applied with an open revert window, and the standard chassis
    /// revert path undoes it from the receipt's own spec.
    #[tokio::test]
    async fn direct_apply_supersedes_and_writes_a_born_applied_receipt() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice pesa 62").await;
        let winner = capture_one(&tree, &pool, "Alice pesa adesso 62").await;

        let receipt = apply_dedup_merge_direct(
            &pool,
            &tree,
            embedder(),
            &winner,
            &loser,
            &DedupMergeHints::default(),
            Some("user:alice".to_owned()),
        )
        .await
        .expect("direct apply");

        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.superseded_by.as_ref().map(FactId::as_str),
            Some(winner.as_str()),
            "the supersede landed in-cycle"
        );
        let (status, recipient, token): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, recipient_id, revert_token FROM structure_proposals \
                 WHERE proposal_id = ?",
        )
        .bind(&receipt.proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied", "born applied — no pending stage");
        assert_eq!(recipient.as_deref(), Some("user:alice"));
        assert!(token.is_some(), "revert window open");

        // The receipt's spec drives the standard revert.
        revert_dedup_merge(&pool, &tree, &receipt.spec)
            .await
            .expect("revert");
        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_none(), "revert reactivated the loser");
    }

    /// The direct apply's disk half: the superseded loser's on-disk
    /// region is excised, the winner's stays, and the settled loser row
    /// carries no offsets (a later revert reactivates it as a pending
    /// render).
    #[tokio::test]
    async fn direct_apply_strips_the_losers_bytes_from_disk() {
        let (dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice pesa 62").await;
        let winner = capture_one(&tree, &pool, "Alice pesa adesso 62").await;

        apply_dedup_merge_direct(
            &pool,
            &tree,
            embedder(),
            &winner,
            &loser,
            &DedupMergeHints::default(),
            None,
        )
        .await
        .expect("direct apply");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
        assert!(
            !page.contains(loser.as_str()),
            "loser marker must be excised: {page}"
        );
        assert!(
            page.contains(winner.as_str()),
            "winner marker must survive: {page}"
        );
        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.region_start.is_none() && row.region_end.is_none(),
            "settled loser row holds no offsets"
        );
    }

    /// The strip is best-effort: a page missing from disk must not fail
    /// the merge — the supersede (the DB half) is what the caller asked
    /// for; the residue question is moot when there are no bytes.
    #[tokio::test]
    async fn direct_apply_survives_a_missing_page_file() {
        let (dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice pesa 62").await;
        let winner = capture_one(&tree, &pool, "Alice pesa adesso 62").await;
        std::fs::remove_file(dir.path().join("wikis/alice/index.md")).unwrap();

        apply_dedup_merge_direct(
            &pool,
            &tree,
            embedder(),
            &winner,
            &loser,
            &DedupMergeHints::default(),
            None,
        )
        .await
        .expect("merge must apply without the page");

        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_some(), "the DB half landed");
    }

    #[tokio::test]
    async fn apply_then_revert_round_trips() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice pesa 62").await;
        let winner = capture_one(&tree, &pool, "Alice pesa adesso 62").await;

        let ctx = json!({
            "loser_fact_id": loser.as_str(),
            "winner_fact_id": winner.as_str(),
        });
        let spec = apply_dedup_merge(&pool, &tree, &ctx, &json!({}))
            .await
            .expect("apply");

        // Round-trip via revert.
        revert_dedup_merge(&pool, &tree, &spec)
            .await
            .expect("revert");

        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_none());
        assert!(row.superseded_by.is_none());
    }

    #[tokio::test]
    async fn revert_rejects_when_chain_moved_on() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "First").await;
        let winner = capture_one(&tree, &pool, "Second").await;
        let newer = capture_one(&tree, &pool, "Third").await;

        // Apply loser → winner.
        let spec = apply_dedup_merge(
            &pool,
            &tree,
            &json!({
                "loser_fact_id": loser.as_str(),
                "winner_fact_id": winner.as_str(),
            }),
            &json!({}),
        )
        .await
        .expect("apply");

        // Simulate the chain growing past our pair: re-activate loser and
        // re-supersede it by `newer` outside of the chassis.
        sqlx::query(
            "UPDATE fact_index SET superseded_at = NULL, superseded_by = NULL WHERE fact_id = ?",
        )
        .bind(loser.as_str())
        .execute(&pool)
        .await
        .unwrap();
        fact_index::mark_superseded(&pool, &loser, &newer)
            .await
            .unwrap();

        let err = revert_dedup_merge(&pool, &tree, &spec)
            .await
            .expect_err("must reject");
        match err {
            RevertError::HandlerData(msg) => assert!(msg.contains("chain has moved on"), "{msg}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn revert_rejects_wrong_variant() {
        let (_dir, tree, pool) = setup().await;
        let bogus = json!({"variant": "paragraph_to_file"});
        let err = revert_dedup_merge(&pool, &tree, &bogus)
            .await
            .expect_err("must reject");
        assert!(matches!(err, RevertError::InvalidPayload(_)));
    }

    // ---------- emit_dedup_merge ----------

    #[tokio::test]
    async fn emit_dedup_merge_writes_pending_row() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Alice has a dog").await;
        let winner = capture_one(&tree, &pool, "Alice owns a dog").await;
        let hints = DedupMergeHints {
            jaccard: Some(0.72),
            source_wiki_id: Some("alice".into()),
            reason: Some("unit test".into()),
        };
        let proposal_id = emit_dedup_merge(&pool, &winner, &loser, &hints, None)
            .await
            .expect("emit");

        let (kind, status, context, timeout_at): (String, String, String, String) = sqlx::query_as(
            "SELECT kind, status, context, timeout_at FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "dedup_merge");
        assert_eq!(status, "pending");
        let ctx: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["winner_fact_id"].as_str(), Some(winner.as_str()),);
        assert_eq!(ctx["loser_fact_id"].as_str(), Some(loser.as_str()),);
        assert!(
            (ctx["jaccard"].as_f64().unwrap() - 0.72).abs() < 1e-3,
            "jaccard must round-trip"
        );
        // 24h default — must parse and be in the future.
        let parsed: chrono::DateTime<chrono::Utc> = timeout_at.parse().unwrap();
        let delta = parsed - chrono::Utc::now();
        assert!(delta.num_hours() >= 23 && delta.num_hours() <= 25);
    }

    #[tokio::test]
    async fn emit_dedup_merge_then_apply_round_trips() {
        let (_dir, tree, pool) = setup().await;
        let loser = capture_one(&tree, &pool, "Bob has a cat").await;
        let winner = capture_one(&tree, &pool, "Bob owns a cat").await;
        let proposal_id =
            emit_dedup_merge(&pool, &winner, &loser, &DedupMergeHints::default(), None)
                .await
                .unwrap();
        // The chassis must accept the emitted proposal end-to-end.
        let outcome =
            crate::proposals::apply_proposal(&pool, &tree, &proposal_id, &json!({}), None, true)
                .await
                .expect("apply");
        assert_eq!(outcome.kind, "dedup_merge");
        let row = fact_index::find_by_id(&pool, &loser)
            .await
            .unwrap()
            .unwrap();
        assert!(row.superseded_at.is_some());
    }
}
