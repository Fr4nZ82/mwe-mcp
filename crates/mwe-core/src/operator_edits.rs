// SPDX-License-Identifier: AGPL-3.0-or-later
//! Operator-facing act-first wrappers for the per-fragment **ACL change**
//! and **validity edit** verbs.
//!
//! The chat ingest path applies these two verbs from a conversational turn
//! (ingest pipeline); the
//! dashboard applies the SAME two verbs from a structured operator form. To
//! keep the act-first orchestration in one place — load nothing extra, write
//! the engine column (probing the promoted fact row first, then the
//! still-buffered capture), compute the disclosure-widening signal, record
//! the audit row, and mint ONE born-applied receipt — the dashboard route
//! is kept thin: it enforces
//! authorisation (it owns the session) and calls one of the two wrappers
//! here.
//!
//! These wrappers do **not** enforce authorisation: the chat path gates on
//! the fact's subject from the recall window; the dashboard route gates on
//! the session (subject-or-admin) and the wiki family (standard only). The
//! wrappers are the shared engine half below that gate.
//!
//! The receipts are `wiki_promote` variants (`acl_change` / `validity_edit`)
//! exactly like the chat path mints, so a receipt reads the same whichever
//! surface produced it.

use sqlx::SqlitePool;

use crate::types::{FactId, Principal};
use crate::{acl, capture_buffer, disclosure_audit, fact_index, promote};

/// Why an operator ACL/validity edit could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum OperatorEditError {
    /// The target fact has no active row on either the promoted
    /// `fact_index` surface or the still-buffered capture surface
    /// (unknown, tombstoned, or vanished between the form render and the
    /// submit). Nothing was changed.
    #[error("fact {0} has no active row on either surface")]
    FactVanished(FactId),
    /// The engine column was written, but the born-applied receipt could
    /// not be minted (or, for an ACL change, an earlier surface write
    /// failed). Maps the direct-promote error class through verbatim so
    /// the caller can tell "applied-but-receipt-failed" from "nothing
    /// changed".
    #[error(transparent)]
    Direct(#[from] promote::DirectPromoteError),
    /// The promoted-fact surface write (`fact_index::set_acl` /
    /// `set_validity`) failed — nothing was changed.
    #[error("fact surface write failed: {0}")]
    FactSurface(#[from] fact_index::FactIndexError),
    /// The still-buffered capture surface write
    /// (`capture_buffer::set_acl` / `set_validity`) failed — nothing was
    /// changed.
    #[error("buffer surface write failed: {0}")]
    BufferSurface(#[from] capture_buffer::CaptureBufferError),
}

/// Apply a per-fragment **ACL change** to one fact act-first, on behalf of a
/// dashboard operator.
///
/// Mirrors the chat path's `apply_plan_acl_changes` for a single target:
///
/// 1. `fact_index::set_acl` (probe the promoted row); on `Ok(None)` fall
///    through to `capture_buffer::set_acl` (the still-buffered capture). A
///    miss on both surfaces is [`OperatorEditError::FactVanished`].
/// 2. Compute [`acl::widens`] against the previous read-set.
/// 3. Record one [`disclosure_audit`] row. An audit failure is logged and
///    the change proceeds with an `audit_id = -1` sentinel (same as the
///    chat path — a missing audit row must never strand an applied change).
/// 4. Mint ONE born-applied `acl_change` receipt via
///    [`promote::emit_acl_change_receipt`].
///
/// `keep_sender` is the fact's cross-user attribution (who captured it),
/// preserved verbatim — a re-share does not rewrite attribution, exactly
/// like the chat verb. `preview` is a short claim preview for the receipt;
/// `actor_id` is the operator's raw session id (the `applied_by` /
/// audit `actor_id`); `recipient` is the addressed user for the receipt.
///
/// # Errors
///
/// [`OperatorEditError::FactVanished`] when the target has no active row;
/// [`OperatorEditError::FactSurface`] / [`OperatorEditError::BufferSurface`]
/// when the surface write fails;
/// [`OperatorEditError::Direct`] when the change applied but the receipt
/// could not be written.
#[allow(clippy::too_many_arguments)]
pub async fn acl_change_operator(
    pool: &SqlitePool,
    fact_id: &FactId,
    wiki_id: &str,
    new_subject: &Principal,
    new_allow: &[Principal],
    keep_sender: Option<&Principal>,
    preview: &str,
    actor_id: &str,
    recipient: Option<String>,
) -> Result<promote::DirectApplied, OperatorEditError> {
    // Surface write: promoted row first, then the still-buffered capture
    // (the id is stable across promotion), EXACTLY like the chat path.
    let (prev, surface) =
        match fact_index::set_acl(pool, fact_id, new_subject, new_allow, keep_sender).await? {
            Some(prev) => (prev, promote::ClosureSurface::Fact),
            None => {
                match capture_buffer::set_acl(pool, fact_id, new_subject, new_allow, keep_sender)
                    .await?
                {
                    Some(prev) => (prev, promote::ClosureSurface::Buffer),
                    None => return Err(OperatorEditError::FactVanished(fact_id.clone())),
                }
            },
        };

    let widening = acl::widens(
        &prev.prev_subject_id,
        &prev.prev_allow_ids,
        new_subject,
        new_allow,
    );
    let audit_id = match disclosure_audit::record(
        pool,
        fact_id,
        wiki_id,
        actor_id,
        &prev,
        new_subject,
        new_allow,
        keep_sender,
        widening,
    )
    .await
    {
        Ok(id) => id,
        Err(err) => {
            // The ACL is already changed; a missing audit row must not
            // strand the change. Log loudly and proceed without the audit
            // anchor (-1 sentinel — nothing downstream looks it up), same as
            // the chat path.
            tracing::error!(error = %err, "operator: acl_change applied but audit row failed");
            -1
        },
    };

    tracing::info!(
        fact_id = %fact_id,
        subject = %new_subject,
        widening,
        surface = surface.as_str(),
        actor = actor_id,
        "operator: ACL CHANGED (dashboard structured action)"
    );

    let applied = promote::AppliedAclChange {
        fact_id: fact_id.clone(),
        wiki_id: wiki_id.to_owned(),
        preview: preview.to_owned(),
        new_subject: new_subject.clone(),
        new_allow: new_allow.to_vec(),
        prev,
        audit_id,
        widening,
        surface,
    };
    let receipt = promote::emit_acl_change_receipt(
        pool,
        std::slice::from_ref(&applied),
        Some(preview),
        Some(actor_id),
        recipient,
    )
    .await?;
    Ok(receipt)
}

/// Apply a per-fragment **validity edit** to one fact act-first, on behalf
/// of a dashboard operator.
///
/// The sibling of [`acl_change_operator`] minus the audit (a validity
/// correction carries no disclosure signal), mirroring the chat path's
/// `apply_plan_validity_edits` for a single target:
///
/// 1. `fact_index::set_validity` (probe the promoted row); on `Ok(None)`
///    fall through to `capture_buffer::set_validity`. A miss on both is
///    [`OperatorEditError::FactVanished`].
/// 2. Mint ONE born-applied `validity_edit` receipt via
///    [`promote::emit_validity_edit_receipt`].
///
/// `valid_from` / `valid_to` are RFC3339 bounds; a `None` leaves that bound
/// unchanged (COALESCE-in-Rust on the surface write). `preview` /
/// `actor_id` / `recipient` carry the same meaning as in
/// [`acl_change_operator`].
///
/// # Errors
///
/// [`OperatorEditError::FactVanished`] when the target has no active row;
/// [`OperatorEditError::FactSurface`] / [`OperatorEditError::BufferSurface`]
/// when the surface write fails;
/// [`OperatorEditError::Direct`] when the edit applied but the receipt
/// could not be written.
pub async fn validity_edit_operator(
    pool: &SqlitePool,
    fact_id: &FactId,
    wiki_id: &str,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    preview: &str,
    actor_id: &str,
    recipient: Option<String>,
) -> Result<promote::DirectApplied, OperatorEditError> {
    let (prev, surface) = match fact_index::set_validity(pool, fact_id, valid_from, valid_to)
        .await?
    {
        Some(prev) => (prev, promote::ClosureSurface::Fact),
        None => match capture_buffer::set_validity(pool, fact_id, valid_from, valid_to).await? {
            Some(prev) => (prev, promote::ClosureSurface::Buffer),
            None => return Err(OperatorEditError::FactVanished(fact_id.clone())),
        },
    };

    tracing::info!(
        fact_id = %fact_id,
        ?valid_from,
        ?valid_to,
        surface = surface.as_str(),
        actor = actor_id,
        "operator: validity EDITED (dashboard structured action)"
    );

    let applied = promote::AppliedValidityEdit {
        fact_id: fact_id.clone(),
        wiki_id: wiki_id.to_owned(),
        preview: preview.to_owned(),
        new_valid_from: valid_from.map(str::to_owned),
        new_valid_to: valid_to.map(str::to_owned),
        prev,
        surface,
    };
    let receipt = promote::emit_validity_edit_receipt(
        pool,
        std::slice::from_ref(&applied),
        Some(preview),
        Some(actor_id),
        recipient,
    )
    .await?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::wiki::WikiTree;

    use tempfile::TempDir;

    async fn make_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
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
    #[tokio::test]
    async fn vanished_target_is_reported_not_panicked() {
        let (_dir, _tree, pool) = setup().await;
        let ghost = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let err = validity_edit_operator(
            &pool,
            &ghost,
            "alice",
            Some("2026-01-01T00:00:00Z"),
            None,
            "ghost",
            "alice",
            None,
        )
        .await
        .expect_err("must error on a vanished target");
        assert!(matches!(err, OperatorEditError::FactVanished(_)));
    }
}
