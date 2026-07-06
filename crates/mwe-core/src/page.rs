// SPDX-License-Identifier: AGPL-3.0-or-later
//! Page-level operations on a standard memory wiki.
//!
//! Today this is the **governed delete-page** primitive
//! (agentic-chat.md §`WikiDeletePage`): a
//! page deletion is partitioned by the per-fragment **sender** axis so one
//! user can never destroy another's contribution —
//!
//! - a fact the `deleter` **sent** is **tombstoned** (they destroy their own
//!   contribution);
//! - a fact sent by **someone else** is **evacuated intact** to its sender's
//!   home wiki ([`crate::promote::apply_fact_refile_direct`], act-first +
//!   born-applied revert receipt) — the `owner`/`allow`/`sender` ACL rides
//!   along untouched, so reading (per-fragment) is unchanged wherever it lands;
//! - when the sender is **gone** (no home wiki — a removed/never-enrolled
//!   principal) the **owner** is the fallback (owner == deleter → tombstone,
//!   else evacuate to the owner's wiki); when neither has a home wiki the fact
//!   is tombstoned (nobody to hand it to).
//!
//! The whole deletion is recorded as **one** born-applied
//! [`crate::bundle`] receipt (`kind = 'bundle'`) wrapping every tombstone +
//! evacuation, so it reverts in block — the admin/deleter (or an admin) can
//! undo it from the dashboard within the revert window. The emptied page husk
//! is dropped by the planner GC on the next compile. Deleting a page is **admin
//! authority** (structure is recall shape, not access — the verb layer gates
//! it); the per-fragment sender axis still governs how each fact on the page is
//! disposed. Smart wikis carry no per-fragment sender and are out of scope here
//! (their single proprietor decides — see
//! smart-wikis.md); this primitive is for
//! **standard** wikis.

use serde_json::json;
use sqlx::SqlitePool;

use crate::bundle::{BundleOp, BundleSpec};
use crate::fact_index::{self, FactIndexError};
use crate::promote::{self, DirectPromoteError};
use crate::proposals::{self, EmitParams, ProposalsError, kind};
use crate::types::{Principal, WikiId};
use crate::wiki::{WikiError, WikiTree};

/// Cross-wiki evacuations always land on the destination wiki's `index.md`
/// (the compilation plan keys pages by bare slug forest-wide, so a named page
/// could collide; the destination re-homes it on its next compile). Mirrors
/// the agentic single-fact move. Shared with the whole-wiki evacuation
/// ([`crate::wiki_delete::delete_wiki_subtree`] in `SenderKeyed` mode).
pub(crate) const EVACUATION_DEST_PAGE: &str = "index.md";

/// Failure of a page-level operation.
#[derive(Debug, thiserror::Error)]
pub enum PageError {
    /// Resolving the wiki or page in the tree failed.
    #[error(transparent)]
    Wiki(#[from] WikiError),
    /// A `fact_index` read or tombstone failed.
    #[error(transparent)]
    FactIndex(#[from] FactIndexError),
    /// An evacuation (cross-wiki refile) failed.
    #[error(transparent)]
    Refile(#[from] DirectPromoteError),
    /// Emitting the bundle receipt failed (the facts already moved on disk).
    #[error(transparent)]
    Receipt(#[from] ProposalsError),
    /// Resolving the owning group's member roster failed.
    #[error("page deletion db: {0}")]
    Db(#[from] sqlx::Error),
}

/// What happened to the facts on a deleted page.
#[derive(Debug, Default, Clone)]
pub struct PageDeletionOutcome {
    /// Facts tombstoned (the deleter's own, or facts with no enrolled home).
    pub facts_tombstoned: u64,
    /// Foreign-authored facts evacuated to their sender's (or owner's) wiki.
    pub facts_evacuated: u64,
    /// The single born-applied `bundle` receipt wrapping every tombstone +
    /// evacuation, for revert. `None` only when the page had no active facts
    /// (a no-op deletion).
    pub bundle_proposal_id: Option<String>,
}

/// How a page deletion disposes of each fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeletionMode {
    /// Default: partition by the per-fragment sender (module docs) — the
    /// deleter's own facts are tombstoned, foreign-authored ones evacuated.
    /// The only mode a non-admin may use.
    #[default]
    SenderKeyed,
    /// **Admin** "delete all facts": tombstone **every** fact regardless of
    /// sender, with no evacuation — the admin is destroying others'
    /// contributions, so the verb layer requires an informed confirmation
    /// (see the `WikiDeletePage` verb).
    TombstoneAll,
}

/// Policy knobs for [`delete_page_direct`]. The non-default `mode` is
/// **admin-only** at the verb layer; the default is the ordinary path.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeletionPolicy {
    /// Per-fact disposition. Defaults to [`DeletionMode::SenderKeyed`].
    pub mode: DeletionMode,
}

/// Delete `page` of `wiki_id`, partitioning its facts by sender (module docs).
///
/// Act-first: tombstones + evacuations apply immediately and are wrapped in
/// **one** born-applied `bundle` receipt that reverts them in block. A page with
/// no active facts is a no-op (no receipt). The admin/deleter (or an admin)
/// keeps a dashboard undo within the revert window; `policy` may override the
/// per-fact disposition (the admin `TombstoneAll` mode, see [`DeletionPolicy`]).
///
/// # Errors
///
/// [`PageError`] on a wiki-resolution, index, refile, or bundle-receipt
/// failure.
pub async fn delete_page_direct(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &WikiId,
    page: &str,
    deleter: &Principal,
    reason: &str,
    policy: DeletionPolicy,
) -> Result<PageDeletionOutcome, PageError> {
    let handle = tree.locate(wiki_id)?;
    let abs_page = handle.abs_dir().join(page);
    let source_path = crate::wiki::workdir_relative_source_path(tree.workdir(), &abs_page);
    let rows = fact_index::find_active_by_source_path(pool, &source_path).await?;

    // Perform each op now, collecting its reversible record into one bundle.
    let mut ops: Vec<BundleOp> = Vec::new();
    let mut out = PageDeletionOutcome::default();
    for row in &rows {
        // The principal responsible for the fact: its sender (provenance) when
        // materialized, else its owner (subject) — the sender-gone fallback.
        // Admin "delete all" collapses every fact onto the tombstone arm.
        let responsible = row.sender_id.as_ref().unwrap_or(&row.owner_id);
        let action = match policy.mode {
            DeletionMode::TombstoneAll => Action::Tombstone,
            DeletionMode::SenderKeyed => decide(responsible, &row.owner_id, deleter, tree),
        };
        match action {
            Action::Tombstone => {
                fact_index::mark_forgotten(pool, &row.fact_id, reason).await?;
                ops.push(BundleOp::Tombstone {
                    fact_id: row.fact_id.as_str().to_owned(),
                });
                out.facts_tombstoned += 1;
            },
            Action::Evacuate(dest) => {
                let spec = promote::apply_fact_refile_collect(
                    pool,
                    tree,
                    &row.fact_id,
                    wiki_id.as_str(),
                    page,
                    &dest,
                    EVACUATION_DEST_PAGE,
                    Some(reason),
                )
                .await?;
                ops.push(BundleOp::Refile { spec });
                out.facts_evacuated += 1;
            },
        }
    }

    // No active facts → nothing to wrap; the husk GC drops the empty page.
    if ops.is_empty() {
        return Ok(out);
    }

    // The single born-applied bundle receipt. Its `context` carries only the
    // receipt-display fields; addressed to the deleter so they (and an admin)
    // can undo it from the dashboard within the window. Deleting a page is
    // admin authority (3·struct), so there is no member vote — the receipt's
    // undo is the only post-deletion lever.
    let bundle = BundleSpec { ops };
    let context = json!({
        "variant": "page_deletion",
        "wiki_id": wiki_id.as_str(),
        "page": page,
    });
    let deleter_bare = match deleter {
        Principal::User(u) => Some(u.as_str()),
        Principal::Group(_) => None,
    };
    let receipt = proposals::emit_applied_proposal(
        pool,
        EmitParams::new(
            kind::BUNDLE,
            context,
            page_deletion_questions(page, bundle.len()),
        )
        .with_recipient(deleter_bare.map(|u| format!("user:{u}"))),
        bundle.to_value().map_err(ProposalsError::Json)?,
        deleter_bare,
    )
    .await?;
    out.bundle_proposal_id = Some(receipt.proposal_id);

    Ok(out)
}

/// Display-only questionnaire stored on a page-deletion bundle receipt (the
/// dashboard renders it; the revert is the one-click admin/deleter undo).
fn page_deletion_questions(page: &str, n_ops: usize) -> serde_json::Value {
    json!([{
        "id": "page_deletion",
        "text": format!("Deleted page `{page}` ({n_ops} facts repatriated or tombstoned)."),
        "options": [
            { "id": "keep", "recommended": true },
            { "id": "undo" },
        ],
    }])
}

pub(crate) enum Action {
    Tombstone,
    /// Evacuate to this home wiki id.
    Evacuate(String),
}

/// The per-fact decision (module docs): tombstone the deleter's own
/// contribution; evacuate a foreign one to its sender's home wiki; fall back to
/// the owner when the sender has no home wiki; tombstone when neither does.
///
/// Shared with [`crate::wiki_delete::delete_wiki_subtree`], whose `SenderKeyed`
/// whole-wiki evacuation partitions every fact in the subtree by exactly this
/// rule. The caller computes `responsible` = the fact's `sender` (provenance)
/// when present, else its `owner` (the sender-gone fallback).
pub(crate) fn decide(
    responsible: &Principal,
    owner: &Principal,
    deleter: &Principal,
    tree: &WikiTree,
) -> Action {
    if responsible == deleter {
        return Action::Tombstone;
    }
    if let Some(dest) = existing_home_wiki(responsible, tree) {
        return Action::Evacuate(dest);
    }
    // Sender gone (no home wiki) → fall back to the owner axis.
    if owner == deleter {
        return Action::Tombstone;
    }
    existing_home_wiki(owner, tree).map_or(Action::Tombstone, Action::Evacuate)
}

/// The home wiki id of a principal **if it exists** in the tree: user `alice` →
/// wiki `alice`, group `famiglia` → wiki `famiglia`. The builtin `global`
/// group has no home wiki.
fn existing_home_wiki(p: &Principal, tree: &WikiTree) -> Option<String> {
    let id = match p {
        Principal::User(id) => id.clone(),
        Principal::Group(id) if id != "global" => id.clone(),
        Principal::Group(_) => return None,
    };
    let wid = WikiId::parse(&id).ok()?;
    tree.locate(&wid).is_ok().then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn seed_user(tree: &WikiTree, id: &str) {
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
             ---\n"
        );
        std::fs::write(dir.join("_meta.md"), meta).unwrap();
    }

    fn user(id: &str) -> Principal {
        format!("user:{id}").parse().unwrap()
    }

    #[test]
    fn decide_partitions_by_sender_with_owner_fallback() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_user(&tree, "franz");
        seed_user(&tree, "morgana");
        let tree = WikiTree::open(dir.path()).unwrap();

        let franz = user("franz");
        let morgana = user("morgana");
        let ghost = user("ghost"); // no home wiki
        let ghost2 = user("ghost2"); // no home wiki

        // sender == deleter → tombstone (you may destroy your own contribution).
        assert!(matches!(
            decide(&franz, &franz, &franz, &tree),
            Action::Tombstone
        ));
        // foreign sender with a home wiki → evacuate there, intact.
        match decide(&morgana, &franz, &franz, &tree) {
            Action::Evacuate(d) => assert_eq!(d, "morgana"),
            Action::Tombstone => panic!("expected evacuation to morgana"),
        }
        // sender gone (no wiki), owner == deleter → tombstone.
        assert!(matches!(
            decide(&ghost, &franz, &franz, &tree),
            Action::Tombstone
        ));
        // sender gone, owner has a wiki ≠ deleter → evacuate to the owner's wiki.
        match decide(&ghost, &morgana, &franz, &tree) {
            Action::Evacuate(d) => assert_eq!(d, "morgana"),
            Action::Tombstone => panic!("expected evacuation to the owner's wiki"),
        }
        // neither sender nor owner has a home wiki → tombstone (nobody to hand
        // it to).
        assert!(matches!(
            decide(&ghost, &ghost2, &franz, &tree),
            Action::Tombstone
        ));
    }
}
