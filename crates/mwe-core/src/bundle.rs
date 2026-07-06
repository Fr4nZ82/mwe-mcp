// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `bundle` proposal kind — wrap many born-applied sub-operations into
//! one revertible receipt.
//!
//! A governed page deletion (the bundle handler)
//! tombstones some facts and evacuates others; on its own each of those is an
//! independent change. The bundle makes them **one atomic, revertible unit**: a
//! single `structure_proposals` row (`kind = 'bundle'`, born-applied via
//! [`crate::proposals::emit_applied_proposal`]) whose `spec` carries the ordered
//! list of sub-operations. A revert — an admin annul, or the deleter's
//! dashboard undo within the 7-day window — replays [`revert_bundle`], undoing
//! every sub-operation in block.
//!
//! The bundle is **assembled** by the producer that performs the
//! sub-operations ([`crate::page::delete_page_direct`] today), which fills a
//! [`BundleSpec`] as it tombstones / evacuates and then emits the receipt with
//! it. This module owns the spec shape and the inverse ([`revert_bundle`]); the
//! `apply` direction is never a chassis path (a bundle is born-applied, never
//! `pending`), so [`crate::proposals::dispatch_apply_kind`] leaves
//! `kind::BUNDLE` unimplemented on purpose — only the revert is wired.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::proposals::RevertError;
use crate::types::FactId;
use crate::wiki::WikiTree;

/// One reversible sub-operation inside a [`BundleSpec`].
///
/// Tagged on the `op` field so the JSON is self-describing in the stored
/// `spec` column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BundleOp {
    /// A fact the deleter tombstoned (their own contribution). The revert
    /// un-tombstones it ([`crate::fact_index::restore_forgotten`]); the next
    /// compile re-renders its marker on the page the row still points at.
    Tombstone {
        /// `fact_id` of the tombstoned row.
        fact_id: String,
    },
    /// A foreign-authored fact evacuated cross-wiki. `spec` is the
    /// `fact_refile` revert spec ([`crate::promote::apply_fact_refile_collect`]);
    /// the revert moves the fact home ([`crate::promote::revert_wiki_promote`]).
    Refile {
        /// The `fact_refile` spec the move produced.
        spec: Value,
    },
}

/// The `spec` payload of a `bundle` receipt: an ordered list of reversible
/// sub-operations.
///
/// Serialized into the `structure_proposals.spec` column at emit; read back by
/// [`revert_bundle`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleSpec {
    /// Sub-operations in application order. [`revert_bundle`] undoes them in
    /// reverse so a later op never depends on an earlier one already gone.
    pub ops: Vec<BundleOp>,
}

impl BundleSpec {
    /// `true` when the bundle wraps no sub-operations (a page with no facts).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of wrapped sub-operations.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.ops.len()
    }

    /// Serialize to the `spec` JSON stored on the receipt.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] — practically unreachable (the shape is owned).
    pub fn to_value(&self) -> std::result::Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

/// Revert a `bundle` receipt: undo every wrapped sub-operation, in reverse
/// application order.
///
/// Wired into [`crate::proposals::revert_proposal`] through its kind dispatch,
/// so a NO-majority vote, an admin annul, or a dashboard undo within the 7-day
/// window all reach here. A single failing sub-op aborts with [`RevertError`]
/// and the proposal stays `applied` (the status flip happens only after this
/// returns `Ok`); because the sub-ops act on independent facts, a re-run is the
/// recovery path (`restore_forgotten` is idempotent).
///
/// # Errors
///
/// [`RevertError`] from any sub-op: a malformed spec, an unparsable `fact_id`,
/// a fact-index failure restoring a tombstone, or a filesystem/data error
/// moving an evacuated fact home.
pub async fn revert_bundle(
    pool: &SqlitePool,
    tree: &WikiTree,
    spec: &Value,
) -> std::result::Result<(), RevertError> {
    let bundle: BundleSpec = serde_json::from_value(spec.clone())
        .map_err(|e| RevertError::InvalidPayload(format!("spec is not a BundleSpec: {e}")))?;
    for op in bundle.ops.iter().rev() {
        match op {
            BundleOp::Tombstone { fact_id } => {
                let fid = FactId::parse(fact_id).map_err(|e| {
                    RevertError::InvalidPayload(format!(
                        "bundle tombstone fact_id {fact_id:?}: {e}"
                    ))
                })?;
                crate::fact_index::restore_forgotten(pool, &fid)
                    .await
                    .map_err(|e| {
                        RevertError::HandlerData(format!("restore tombstone {fact_id}: {e}"))
                    })?;
            },
            BundleOp::Refile { spec } => {
                crate::promote::revert_wiki_promote(pool, tree, spec).await?;
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_spec_round_trips_through_json() {
        let spec = BundleSpec {
            ops: vec![
                BundleOp::Tombstone {
                    fact_id: "0190a0c8-0000-7000-8000-000000000001".to_owned(),
                },
                BundleOp::Refile {
                    spec: serde_json::json!({ "variant": "fact_refile", "dest_wiki_id": "morgana" }),
                },
            ],
        };
        let value = spec.to_value().expect("to_value");
        // The op tag is self-describing in the stored column.
        assert_eq!(value["ops"][0]["op"], "tombstone");
        assert_eq!(value["ops"][1]["op"], "refile");
        let back: BundleSpec = serde_json::from_value(value).expect("from_value");
        assert_eq!(back, spec);
        assert_eq!(back.len(), 2);
        assert!(!back.is_empty());
    }

    #[tokio::test]
    async fn revert_bundle_rejects_a_non_bundle_spec() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        let tree =
            WikiTree::open(Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path())
                .expect("tree");
        let err = revert_bundle(&pool, &tree, &serde_json::json!({ "not": "a bundle" }))
            .await
            .expect_err("must reject");
        assert!(matches!(err, RevertError::InvalidPayload(_)));
    }
}
