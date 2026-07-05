// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only soft-delete of a whole memory-wiki subtree.
//!
//! Deleting a wiki is the most destructive operator action mwe-mcp exposes.
//! Under "ACL lives only in the fact" deleting *structure* (a page or a wiki) is
//! an **admin** act ([the write-authority model](../../../wiki/concepts/identity-and-acl.md)),
//! and on a deletion the admin chooses how the subtree's facts are disposed —
//! the same two arms as the per-page governed delete ([`crate::page`]):
//!
//! - [`crate::page::DeletionMode::SenderKeyed`] — **move**: each fact is
//!   partitioned by its per-fragment `sender`, so no one's contribution is
//!   destroyed. A fact the `deleter` sent is tombstoned (their own); a
//!   foreign-authored one is **evacuated intact** to its sender's home wiki (the
//!   `owner` is the fallback when the sender has no home), carrying its
//!   `owner`/`allow`/`sender` ACL untouched — reading is per-fragment, so it
//!   keeps its audience wherever it lands. Reuses [`crate::page::decide`] + the
//!   cross-wiki refile engine.
//! - [`crate::page::DeletionMode::TombstoneAll`] — **tombstone them all**: every
//!   fact in the subtree is tombstoned regardless of sender, destroying others'
//!   contributions; the verb layer requires an informed confirmation.
//!
//! Either way the on-disk directory is then **moved** into `<workdir>/trash/`,
//! never `rm -rf`'d — an operator who deleted the wrong wiki can move the
//! directory back and let the watcher re-index it. Tombstoned rows survive as
//! audit tombstones (visible under the dashboard "include inactive" filter);
//! evacuated facts are already safe in their senders' wikis.
//!
//! Identity wikis (`wiki-user` / `wiki-group`) are refused here: they are an
//! account's autobiographical store and are removed through the user/group
//! deletion flow instead.
//!
//! Sub-wikis travel with their parent — the disposition pass and the directory
//! move both cover every wiki id under the target. The trash root is a sibling
//! of `<workdir>/wikis/`, so a trashed subtree never reappears in
//! [`WikiTree::walk`].

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::fact_index;
use crate::page::{self, Action, DeletionMode};
use crate::promote::{self, DirectPromoteError};
use crate::types::{Principal, WikiId};
use crate::wiki::{
    DiscoveredWiki, GROUP_IDENTITY_WIKI_TYPE, IDENTITY_WIKI_TYPE, WikiError, WikiTree,
};

/// `deleted_reason` stamped on every fact the deleted subtree carried.
pub const DELETE_REASON: &str = "wiki_deleted";

/// What the deletion touched — surfaced to the operator and the logs.
#[derive(Debug, Clone)]
pub struct WikiDeleteReport {
    /// The wiki the operator targeted.
    pub wiki_id: WikiId,
    /// Number of wikis removed (target + descendants).
    pub wikis_removed: usize,
    /// Number of fact rows tombstoned across the subtree (the deleter's own +
    /// homeless facts in `SenderKeyed` mode; **every** fact in `TombstoneAll`).
    pub facts_tombstoned: u64,
    /// Number of foreign-authored facts evacuated to their senders' (or owners')
    /// wikis. Non-zero only in `SenderKeyed` mode; always 0 for `TombstoneAll`.
    pub facts_evacuated: u64,
    /// Where the directory subtree now lives under `<workdir>/trash/`.
    pub trash_dir: PathBuf,
}

/// Failure modes of [`delete_wiki_subtree`].
#[derive(Debug, thiserror::Error)]
pub enum WikiDeleteError {
    /// No wiki carries the requested id.
    #[error("wiki {0:?} not found")]
    NotFound(WikiId),
    /// The target is an identity wiki — refused on purpose.
    #[error("refusing to delete identity wiki {0:?} (type {1}); remove the user/group instead")]
    Identity(WikiId, String),
    /// Tree traversal / `_meta.md` parse failure.
    #[error("wiki tree: {0}")]
    Wiki(#[from] WikiError),
    /// Tombstone pass failure.
    #[error("fact index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),
    /// Evacuating a foreign-authored fact to its sender's wiki (the
    /// `SenderKeyed` move arm) failed.
    #[error("evacuating a fact: {0}")]
    Refile(#[from] DirectPromoteError),
    /// Moving the directory into the trash failed.
    #[error("moving {path} to trash: {source}")]
    Move {
        /// The path the move failed on.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// True for the two identity wiki types we refuse to delete here.
#[must_use]
pub fn is_identity_type(wiki_type: &str) -> bool {
    wiki_type == IDENTITY_WIKI_TYPE || wiki_type == GROUP_IDENTITY_WIKI_TYPE
}

/// The target wiki plus every descendant (inclusive), in walk order.
///
/// # Errors
///
/// [`WikiError::WikiNotFound`] when no node carries `target`; other tree
/// traversal / `_meta.md` parse failures otherwise.
pub fn collect_subtree(tree: &WikiTree, target: &WikiId) -> Result<Vec<DiscoveredWiki>, WikiError> {
    let all = tree.walk()?;
    let Some(root) = all.iter().find(|d| &d.meta.wiki_id == target) else {
        return Err(WikiError::WikiNotFound {
            id: target.clone(),
            path: PathBuf::new(),
        });
    };
    let root_abs = root.abs_dir.clone();
    // `Path::starts_with` is component-wise, so `…/alice` never captures a
    // sibling `…/alice2` — only the genuine subtree travels.
    Ok(all
        .into_iter()
        .filter(|d| d.abs_dir.starts_with(&root_abs))
        .collect())
}

/// Soft-delete the wiki `target` and its whole subtree (see module docs),
/// disposing of its facts by `mode` on the `deleter`'s authority.
///
/// `mode` is the admin's choice: [`DeletionMode::SenderKeyed`] **moves** every
/// foreign-authored fact to its sender's home wiki (tombstoning only the
/// deleter's own + homeless facts); [`DeletionMode::TombstoneAll`] **tombstones
/// every** fact. Disposal runs before the directory move so the refile engine
/// still sees the source pages.
///
/// # Errors
///
/// [`WikiDeleteError`] — unknown id, identity-wiki refusal, tree/engine
/// failure, a fact evacuation, or the directory move.
pub async fn delete_wiki_subtree(
    pool: &SqlitePool,
    tree: &WikiTree,
    target: &WikiId,
    deleter: &Principal,
    mode: DeletionMode,
) -> Result<WikiDeleteReport, WikiDeleteError> {
    let subtree = collect_subtree(tree, target)?;
    // `collect_subtree` guarantees the target is present.
    let root = subtree
        .iter()
        .find(|d| &d.meta.wiki_id == target)
        .ok_or_else(|| WikiDeleteError::NotFound(target.clone()))?;
    if is_identity_type(&root.meta.wiki_type) {
        return Err(WikiDeleteError::Identity(
            target.clone(),
            root.meta.wiki_type.clone(),
        ));
    }
    let root_abs = root.abs_dir.clone();

    // Dispose of the facts first (DB + cross-wiki refile), then move the husk
    // (fs): recall goes quiet — or the fact lands in its home wiki — the instant
    // the rows flip/move, while the source directory still exists for the refile
    // engine to edit; a failure leaves a retry-able subtree rather than live
    // rows pointing at a vanished directory.
    let mut facts_tombstoned = 0u64;
    let mut facts_evacuated = 0u64;
    for d in &subtree {
        let wiki_id = d.meta.wiki_id.as_str();
        match mode {
            DeletionMode::TombstoneAll => {
                facts_tombstoned +=
                    fact_index::mark_forgotten_in_wiki(pool, wiki_id, DELETE_REASON).await?;
            },
            DeletionMode::SenderKeyed => {
                for row in fact_index::find_active_in_wiki(pool, wiki_id).await? {
                    // The principal responsible for the fact: its sender
                    // (provenance) when present, else its owner — the same
                    // partition the per-page governed delete applies.
                    let responsible = row.sender_id.as_ref().unwrap_or(&row.owner_id);
                    match page::decide(responsible, &row.owner_id, deleter, tree) {
                        Action::Tombstone => {
                            fact_index::mark_forgotten(pool, &row.fact_id, DELETE_REASON).await?;
                            facts_tombstoned += 1;
                        },
                        Action::Evacuate(dest) => {
                            promote::apply_fact_refile_collect(
                                pool,
                                tree,
                                &row.fact_id,
                                wiki_id,
                                page_basename(&row.source_path),
                                &dest,
                                page::EVACUATION_DEST_PAGE,
                                Some(DELETE_REASON),
                            )
                            .await?;
                            facts_evacuated += 1;
                        },
                    }
                }
            },
        }
    }

    let trash_root = tree.workdir().join("trash");
    std::fs::create_dir_all(&trash_root).map_err(|source| WikiDeleteError::Move {
        path: trash_root.clone(),
        source,
    })?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let trash_dir = trash_root.join(format!("{}__{stamp}", target.as_str()));
    std::fs::rename(&root_abs, &trash_dir).map_err(|source| WikiDeleteError::Move {
        path: root_abs.clone(),
        source,
    })?;

    Ok(WikiDeleteReport {
        wiki_id: target.clone(),
        wikis_removed: subtree.len(),
        facts_tombstoned,
        facts_evacuated,
        trash_dir,
    })
}

/// The page filename (last path component) of a workdir-relative source path:
/// `wikis/famiglia/vacanze.md` → `vacanze.md` — the `source_page` the refile
/// engine edits.
fn page_basename(source_path: &str) -> &str {
    source_path.rsplit('/').next().unwrap_or(source_path)
}

#[cfg(test)]
mod tests {
    use super::{collect_subtree, delete_wiki_subtree, is_identity_type, page_basename};
    use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
    use crate::embedder::{Embedder, FakeEmbedder};
    use crate::fact_index;
    use crate::page::DeletionMode;
    use crate::types::{FactId, Principal, WikiId};
    use crate::wiki::{META_FILENAME, WikiTree};
    use sqlx::SqlitePool;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("fake", 4))
    }

    /// Seed a wiki directory the tree can locate (`_meta.md` with a concrete
    /// `acl_default` so `walk` parses it without a parent chain).
    fn seed(tree: &WikiTree, id: &str, wiki_type: &str) {
        write_meta(&tree.wikis_dir().join(id), &meta(id, wiki_type, None));
    }

    /// Capture a fact into `wiki` owned by `user:<owner_user>` (sender unset, so
    /// the owner is the responsible principal the evacuation keys on) — writes
    /// the page on disk so the refile can read it.
    async fn capture_owned(
        tree: &WikiTree,
        pool: &SqlitePool,
        emb: Arc<dyn Embedder>,
        wiki: &str,
        owner_user: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: format!("user:{owner_user}").parse::<Principal>().unwrap(),
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
        let outcome = wiki_capture(tree, pool, emb, req).await.unwrap();
        match outcome.action {
            CaptureAction::Captured { .. } => outcome.fact_id,
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    /// Minimal valid `_meta.md` (concrete `acl_default`, so `walk` parses it
    /// without a parent-chain).
    fn meta(wiki_id: &str, wiki_type: &str, parent: Option<&str>) -> String {
        let slug = wiki_id.rsplit('-').next().unwrap_or(wiki_id);
        let parent_line = parent.map_or_else(
            || "parent_wiki_id: null\n".to_owned(),
            |p| format!("parent_wiki_id: {p}\n"),
        );
        format!(
            "---\n\
             wiki_id: {wiki_id}\n\
             wiki_type: {wiki_type}\n\
             {parent_line}\
             slug: {slug}\n\
             title: {wiki_id}\n\
             acl_default: 'user:owner'\n\
             ---\n"
        )
    }

    fn write_meta(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(META_FILENAME), body).unwrap();
    }

    #[test]
    fn subtree_includes_descendants_but_not_siblings() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // `acme` (target) + a sub-wiki, plus a sibling `acme2` whose path
        // shares the `acme` *prefix* but not the `acme` path *component*.
        write_meta(
            &tree.wikis_dir().join("acme"),
            &meta("acme", "wiki-cliente", None),
        );
        write_meta(
            &tree.wikis_dir().join("acme/proj"),
            &meta("acme-proj", "wiki-cliente", Some("acme")),
        );
        write_meta(
            &tree.wikis_dir().join("acme2"),
            &meta("acme2", "wiki-cliente", None),
        );

        let ids: Vec<String> = collect_subtree(&tree, &WikiId::parse("acme").unwrap())
            .unwrap()
            .iter()
            .map(|d| d.meta.wiki_id.as_str().to_owned())
            .collect();

        assert!(ids.contains(&"acme".to_owned()), "{ids:?}");
        assert!(ids.contains(&"acme-proj".to_owned()), "{ids:?}");
        assert!(
            !ids.contains(&"acme2".to_owned()),
            "component-wise prefix must exclude the sibling: {ids:?}"
        );
    }

    #[test]
    fn unknown_wiki_is_not_found() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_meta(
            &tree.wikis_dir().join("acme"),
            &meta("acme", "wiki-cliente", None),
        );
        assert!(collect_subtree(&tree, &WikiId::parse("ghost").unwrap()).is_err());
    }

    #[test]
    fn identity_types_are_refused() {
        assert!(is_identity_type("wiki-user"));
        assert!(is_identity_type("wiki-group"));
        assert!(!is_identity_type("wiki-cliente"));
        assert!(!is_identity_type("custom-cliente"));
    }

    #[test]
    fn page_basename_is_the_last_path_component() {
        assert_eq!(page_basename("wikis/famiglia/vacanze.md"), "vacanze.md");
        assert_eq!(page_basename("index.md"), "index.md");
    }

    #[tokio::test]
    async fn senderkeyed_move_evacuates_foreign_and_tombstones_own() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "acme", "wiki-cliente"); // a deletable container
        seed(&tree, "bob", "wiki-user"); // a home wiki to evacuate into
        let tree = WikiTree::open(dir.path()).unwrap(); // pick up the seeded wikis
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        let emb = embedder();

        // bob owns a note in his own wiki (so bob/index.md exists as a dest),
        // plus a fact that landed in acme; franz (the deleter) has one too.
        capture_owned(&tree, &pool, emb.clone(), "bob", "bob", "Bob's own note").await;
        let bob_fact = capture_owned(&tree, &pool, emb.clone(), "acme", "bob", "About bob").await;
        capture_owned(&tree, &pool, emb, "acme", "franz", "About franz").await;

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("acme").unwrap(),
            &Principal::User("franz".to_owned()),
            DeletionMode::SenderKeyed,
        )
        .await
        .expect("delete acme (move)");

        // Move: bob's foreign fact is evacuated to bob, only franz's own is
        // tombstoned — no one's contribution is destroyed.
        assert_eq!(
            report.facts_evacuated, 1,
            "bob's fact evacuated, not destroyed"
        );
        assert_eq!(
            report.facts_tombstoned, 1,
            "only the deleter's own tombstoned"
        );
        assert_eq!(report.wikis_removed, 1);
        let row = fact_index::find_by_id(&pool, &bob_fact)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "bob", "the evacuated fact now lives in bob");

        // The husk moved to trash, not erased.
        assert!(!tree.wikis_dir().join("acme").exists());
        assert!(report.trash_dir.exists());
    }

    #[tokio::test]
    async fn tombstone_all_tombstones_everyone_without_evacuating() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "acme", "wiki-cliente");
        let tree = WikiTree::open(dir.path()).unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        let emb = embedder();

        // Two foreign facts (neither is the deleter): tombstone-all destroys
        // both in place, never evacuating.
        capture_owned(&tree, &pool, emb.clone(), "acme", "bob", "About bob").await;
        capture_owned(&tree, &pool, emb, "acme", "morgana", "About morgana").await;

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("acme").unwrap(),
            &Principal::User("franz".to_owned()),
            DeletionMode::TombstoneAll,
        )
        .await
        .expect("delete acme (tombstone all)");

        assert_eq!(report.facts_tombstoned, 2, "every fact tombstoned");
        assert_eq!(report.facts_evacuated, 0, "tombstone-all never evacuates");
        assert!(!tree.wikis_dir().join("acme").exists());
    }
}
