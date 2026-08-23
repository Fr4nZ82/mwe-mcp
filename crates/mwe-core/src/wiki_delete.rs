// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only soft-delete of a whole memory-wiki subtree.
//!
//! Deleting a wiki is the most destructive operator action mwe-mcp exposes.
//! Under "ACL lives only in the fact" deleting *structure* (a page or a wiki) is
//! an **admin** act (the write-authority model),
//! and on a deletion the admin chooses how the subtree's facts are disposed.
//! Two arms are shared with the per-page governed delete ([`crate::page`]); the
//! third is whole-wiki only:
//!
//! - [`crate::page::DeletionMode::Dissolve`] — **the default**: destroy the
//!   *structure*, keep every fact. Nothing is tombstoned. Each fact goes back
//!   into the capture buffer, so the next placement pass **decides where it
//!   belongs** corpus-wide instead of letting it inherit the page it happened
//!   to sit on. The one fact a dissolve cannot
//!   keep is one with no live home anywhere (a `global`-owned fact whose sender
//!   is gone and whose deleter has none): leaving its row pointing into the
//!   trash would drop it out of the plan's input and strand it invisibly, so it
//!   is tombstoned — and **counted** in `facts_tombstoned`, never silent.
//! - [`crate::page::DeletionMode::SenderKeyed`] — **return to each author**: each fact is
//!   partitioned by its per-fragment `sender`, so no one's contribution is
//!   destroyed. A fact the `deleter` sent is tombstoned (their own); a
//!   foreign-authored one is **evacuated intact** to its sender's home wiki (the
//!   `subject` is the fallback when the sender has no home), carrying its
//!   `subject`/`allow`/`sender` ACL untouched — reading is per-fragment, so it
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
//! Identity wikis (`wiki-user` / `wiki-group`) are refused here **while their
//! principal is enrolled**: they are an account's autobiographical store and
//! are removed through the user/group deletion flow instead. Once the
//! user/group is gone the wiki is an orphan (user deletion keeps the memory —
//! the sender-scrub invariant) and that flow can never be re-run for it, so
//! the admin may delete it here like any other wiki.
//!
//! ## A smart wiki is deleted, not disposed of
//!
//! The three modes partition **facts** by authorship. A smart wiki has none:
//! its content lives in `wiki_sections`, whose ACL sits at wiki level in
//! `_meta.md`, so there is no per-fragment sender to hand a fragment back to
//! and nothing to evacuate. Deleting one therefore hard-drops every section
//! row and its `smart_wikis` registry row, **in every mode** — a smart wiki
//! deleted is deleted, it does not inherit the standard wiki's disposal
//! choice.
//!
//! Two things make that safe rather than harsh. Both tables are
//! **projections** of the `.md` files ([`crate::sections`]), so moving the
//! directory back out of the trash lets the watcher rebuild them — the same
//! recourse a standard wiki gets. And dropping the registry row is what makes the
//! deletion take effect for readers *at once*: the readable-wiki filter is
//! built from that table, so a surviving row would keep serving a deleted
//! wiki's content until the next safety-net tick.
//!
//! Sub-wikis travel with their parent — the disposition pass and the directory
//! move both cover every wiki id under the target. The trash root is a sibling
//! of `<workdir>/wikis/`, so a trashed subtree never reappears in
//! [`WikiTree::walk`].

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::capture_buffer;
use crate::enrollment;
use crate::fact_index;
use crate::page::{self, Action, DeletionMode};
use crate::promote::DirectPromoteError;
use crate::sections;
use crate::types::{Principal, WikiId};
use crate::wiki::{
    DiscoveredWiki, GROUP_IDENTITY_WIKI_TYPE, IDENTITY_WIKI_TYPE, WikiError, WikiTree,
};

/// `deleted_reason` stamped on every fact the deleted subtree carried.
pub const DELETE_REASON: &str = "wiki_deleted";

/// Refile reason stamped on a fact a **dissolve** freed.
///
/// Distinct from [`DELETE_REASON`] on purpose: nothing was deleted, the fact
/// only left a structure that no longer exists, and the audit trail should
/// say so.
pub const DISSOLVE_REASON: &str = "wiki_dissolved";

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
    /// Number of foreign-authored facts handed back: they went into the
    /// capture buffer, and the next placement pass writes each one wherever
    /// its subject lives. Non-zero only in `SenderKeyed` mode; always 0 for
    /// `TombstoneAll`.
    pub facts_evacuated: u64,
    /// Number of facts a `Dissolve` freed into the capture buffer, so the
    /// next placement pass re-decides where each belongs. Non-zero only in
    /// `Dissolve` mode.
    pub facts_unplaced: u64,
    /// Number of `wiki_sections` rows hard-dropped across the subtree — a
    /// smart wiki's whole content. Disposal-independent: the modes partition
    /// facts by authorship, which a section does not have.
    pub sections_dropped: u64,
    /// Number of `page_card` rows dropped across the subtree — the standard
    /// wiki's twin of `sections_dropped`, and a projection in the same sense.
    pub page_cards_dropped: u64,
    /// Number of `link_key` rows dropped across the subtree. Same class as
    /// the cards: derived from the pages, and nothing else ever walks a wiki
    /// that is gone.
    pub link_keys_dropped: u64,
    /// Where the directory subtree now lives under `<workdir>/trash/`.
    pub trash_dir: PathBuf,
}

/// Failure modes of [`delete_wiki_subtree`].
#[derive(Debug, thiserror::Error)]
pub enum WikiDeleteError {
    /// No wiki carries the requested id.
    #[error("wiki {0:?} not found")]
    NotFound(WikiId),
    /// Handing a freed fact back to the capture buffer failed.
    #[error(transparent)]
    Buffer(#[from] crate::capture_buffer::CaptureBufferError),
    /// The target is a living principal's identity wiki — refused on purpose.
    #[error("refusing to delete identity wiki {0:?} (type {1}); remove the user/group instead")]
    Identity(WikiId, String),
    /// Reading the persisted compilation plan failed. Raised only by the
    /// `Dissolve` arm, and only **before** it touches a fact, so the subtree
    /// is left intact and the operator can retry.
    #[error("compilation plan: {0}")]
    Plan(#[from] crate::planner::PlannerError),
    /// Checking whether the identity wiki's principal is still enrolled failed.
    #[error("enrollment lookup: {0}")]
    Enrollment(#[from] sqlx::Error),
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
    /// Dropping the subtree's smart-wiki sections or registry rows failed.
    #[error("wiki sections: {0}")]
    Sections(#[from] sections::SectionError),
    /// Dropping the subtree's page cards failed.
    #[error("page cards: {0}")]
    PageCards(#[from] crate::page_card::PageCardError),
    /// Dropping the subtree's clause keys failed.
    #[error("link keys: {0}")]
    LinkKeys(#[from] crate::link_key::LinkKeyError),
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

/// True for the two identity wiki types we refuse to delete while their
/// principal is enrolled.
#[must_use]
pub fn is_identity_type(wiki_type: &str) -> bool {
    wiki_type == IDENTITY_WIKI_TYPE || wiki_type == GROUP_IDENTITY_WIKI_TYPE
}

/// The principal a **root** identity wiki names (`wiki-user` → `user:<id>`,
/// `wiki-group` → `group:<id>`); `None` for non-identity types.
///
/// Same id convention as `WikiTree::resolve_scope_principal`, exposed so the
/// delete guard and the dashboard can ask "whose identity wiki is this?"
/// without walking the tree.
#[must_use]
pub fn identity_principal(wiki_type: &str, wiki_id: &WikiId) -> Option<Principal> {
    match wiki_type {
        IDENTITY_WIKI_TYPE => Some(Principal::User(wiki_id.as_str().to_owned())),
        GROUP_IDENTITY_WIKI_TYPE => Some(Principal::Group(wiki_id.as_str().to_owned())),
        _ => None,
    }
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
#[allow(
    clippy::too_many_lines,
    reason = "one linear disposition pass per mode, then the husk move; splitting hides the order the guarantees depend on"
)]
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
    if let Some(principal) = identity_principal(&root.meta.wiki_type, &root.meta.wiki_id) {
        // The refusal protects a *living* account's store. An orphan —
        // its user/group already removed — has no other deletion path,
        // so it falls through to the normal admin delete.
        if enrollment::principal_exists(pool, &principal).await? {
            return Err(WikiDeleteError::Identity(
                target.clone(),
                root.meta.wiki_type.clone(),
            ));
        }
    }
    let root_abs = root.abs_dir.clone();

    // Dispose of the facts first (DB + cross-wiki refile), then move the husk
    // (fs): recall goes quiet — or the fact lands in its home wiki — the instant
    // the rows flip/move, while the source directory still exists for the refile
    // engine to edit; a failure leaves a retry-able subtree rather than live
    // rows pointing at a vanished directory.
    let mut facts_tombstoned = 0u64;
    let mut facts_evacuated = 0u64;
    let mut facts_unplaced = 0u64;
    let mut sections_dropped = 0u64;
    let mut page_cards_dropped = 0u64;
    let mut link_keys_dropped = 0u64;
    let now = chrono::Utc::now().to_rfc3339();
    for d in &subtree {
        let wiki_id = d.meta.wiki_id.as_str();
        match mode {
            DeletionMode::TombstoneAll => {
                facts_tombstoned +=
                    fact_index::mark_forgotten_in_wiki(pool, wiki_id, DELETE_REASON).await?;
            },
            DeletionMode::Dissolve => {
                // Structure goes, content stays. Every fact goes back into the
                // capture buffer — never tombstoned — so the next placement
                // pass decides where it belongs across the whole corpus
                // instead of it inheriting the page it happened to sit on.
                for row in fact_index::find_active_in_wiki(pool, wiki_id).await? {
                    capture_buffer::rebuffer_fact(pool, &row.fact_id, &now).await?;
                    facts_unplaced += 1;
                }
            },
            DeletionMode::SenderKeyed => {
                for row in fact_index::find_active_in_wiki(pool, wiki_id).await? {
                    // The principal responsible for the fact: its sender
                    // (provenance) when present, else its subject — the same
                    // partition the per-page governed delete applies.
                    let responsible = row.sender_id.as_ref().unwrap_or(&row.subject_id);
                    match page::decide(responsible, &row.subject_id, deleter, tree) {
                        Action::Tombstone => {
                            fact_index::mark_forgotten(pool, &row.fact_id, DELETE_REASON).await?;
                            facts_tombstoned += 1;
                        },
                        Action::Evacuate => {
                            capture_buffer::rebuffer_fact(pool, &row.fact_id, &now).await?;
                            facts_evacuated += 1;
                        },
                    }
                }
            },
        }

        // Whatever the mode just did to the facts, a smart wiki's own content
        // goes with it — see the module docs. Unconditional on purpose: on a
        // standard wiki both calls are no-ops, and on one that *used* to be
        // smart they collect the rows a `smart: false` edit stranded. The
        // registry row goes here, not at the next sweep, so recall stops
        // serving the wiki the moment it is deleted.
        sections_dropped += sections::drop_wiki_sections(pool, wiki_id).await?;
        sections::remove_smart_wiki(pool, wiki_id).await?;
        // The standard wiki's twin of the same rule: its page cards and the
        // clause keys read off its prose are projections too, and the sweeps
        // that refresh them only walk wikis still on disk, so nothing else
        // would ever collect them.
        page_cards_dropped += crate::page_card::drop_wiki(pool, wiki_id).await?;
        link_keys_dropped += crate::link_key::drop_wiki(pool, wiki_id).await?;
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
        facts_unplaced,
        sections_dropped,
        page_cards_dropped,
        link_keys_dropped,
        trash_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::{collect_subtree, delete_wiki_subtree, is_identity_type};
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

    /// Capture a fact into `wiki` whose SUBJECT is `user:<subject_user>` (sender unset, so
    /// the subject is the responsible principal the evacuation keys on) — writes
    /// the page on disk so the refile can read it.
    async fn capture_with_subject(
        tree: &WikiTree,
        pool: &SqlitePool,
        emb: Arc<dyn Embedder>,
        wiki: &str,
        subject_user: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: format!("user:{subject_user}").parse::<Principal>().unwrap(),
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

        // a note about bob lives in his own wiki (so bob/cucina.md exists as a dest),
        // plus a fact that landed in acme; franz (the deleter) has one too.
        capture_with_subject(&tree, &pool, emb.clone(), "bob", "bob", "Bob's own note").await;
        let bob_fact =
            capture_with_subject(&tree, &pool, emb.clone(), "acme", "bob", "About bob").await;
        capture_with_subject(&tree, &pool, emb, "acme", "franz", "About franz").await;

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
        assert!(
            fact_index::find_by_id(&pool, &bob_fact)
                .await
                .unwrap()
                .is_none(),
            "the handed-back fact is un-written, waiting to be placed again"
        );
        let queued = crate::capture_buffer::find_all_buffered(&pool, 10)
            .await
            .expect("queue");
        assert!(
            queued.iter().any(|c| c.capture_id == bob_fact),
            "bob's fact is in the queue: {queued:?}"
        );

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
        capture_with_subject(&tree, &pool, emb.clone(), "acme", "bob", "About bob").await;
        capture_with_subject(&tree, &pool, emb, "acme", "morgana", "About morgana").await;

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

    #[tokio::test]
    async fn living_identity_wiki_is_refused() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "bob", "wiki-user");
        let tree = WikiTree::open(dir.path()).unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('bob', 0)")
            .execute(&pool)
            .await
            .unwrap();

        let err = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("bob").unwrap(),
            &Principal::User("franz".to_owned()),
            DeletionMode::TombstoneAll,
        )
        .await
        .expect_err("bob is enrolled — his identity wiki must be refused");

        assert!(
            matches!(err, super::WikiDeleteError::Identity(..)),
            "expected the identity refusal, got: {err}"
        );
        assert!(tree.wikis_dir().join("bob").exists(), "nothing moved");
    }

    #[tokio::test]
    async fn orphan_identity_wiki_is_deletable() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // An identity wiki whose user is NOT enrolled — the leftover of a
        // user deletion (memory outlives the identity). No other flow can
        // remove it, so the admin delete must accept it.
        seed(&tree, "ghost", "wiki-user");
        let tree = WikiTree::open(dir.path()).unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        let emb = embedder();
        capture_with_subject(&tree, &pool, emb, "ghost", "ghost", "A ghost's note").await;

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("ghost").unwrap(),
            &Principal::User("franz".to_owned()),
            DeletionMode::TombstoneAll,
        )
        .await
        .expect("orphan identity wiki deletes like any other");

        assert_eq!(report.wikis_removed, 1);
        assert_eq!(report.facts_tombstoned, 1);
        assert!(!tree.wikis_dir().join("ghost").exists());
        assert!(report.trash_dir.exists(), "husk in trash, recoverable");
    }

    // ---------- dissolve ----------

    #[tokio::test]
    async fn dissolve_returns_every_fact_to_the_queue() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "alice", "wiki-user");
        seed(&tree, "dossier", "wiki-tech");
        let tree = WikiTree::open(dir.path()).unwrap();

        let f1 =
            capture_with_subject(&tree, &pool, embedder(), "dossier", "alice", "first note").await;
        let f2 =
            capture_with_subject(&tree, &pool, embedder(), "dossier", "alice", "second note").await;

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("dossier").unwrap(),
            &Principal::User("admin".to_owned()),
            DeletionMode::Dissolve,
        )
        .await
        .expect("dissolve");

        // Nothing destroyed: a dissolve never tombstones.
        assert_eq!(report.facts_tombstoned, 0);
        assert_eq!(report.facts_unplaced, 2);
        assert!(report.trash_dir.exists(), "the husk went to trash");

        // Both claims are back in the queue under their own ids, and no
        // `fact_index` row is left pointing into the trash.
        let queued: Vec<String> = crate::capture_buffer::find_all_buffered(&pool, 10)
            .await
            .expect("queue")
            .into_iter()
            .map(|c| c.capture_id.as_str().to_owned())
            .collect();
        for fid in [&f1, &f2] {
            assert!(
                fact_index::find_by_id(&pool, fid).await.unwrap().is_none(),
                "the written row is gone — the claim is un-written, not tombstoned"
            );
            assert!(
                queued.contains(&fid.as_str().to_owned()),
                "{fid} must be waiting in the queue: {queued:?}"
            );
        }
    }

    #[tokio::test]
    async fn dissolve_queues_even_a_fact_with_nowhere_live_to_go() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "dossier", "wiki-tech");
        let tree = WikiTree::open(dir.path()).unwrap();
        // Subject `ghost` has no wiki, and neither does the deleter. The
        // queue does not need one: a claim nobody can home simply keeps
        // waiting there until somebody enrols.
        let fid =
            capture_with_subject(&tree, &pool, embedder(), "dossier", "ghost", "homeless").await;

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("dossier").unwrap(),
            &Principal::User("nobody".to_owned()),
            DeletionMode::Dissolve,
        )
        .await
        .expect("dissolve");

        assert_eq!(report.facts_unplaced, 1);
        assert_eq!(report.facts_tombstoned, 0, "a dissolve destroys nothing");
        assert!(
            fact_index::find_by_id(&pool, &fid).await.unwrap().is_none(),
            "the row is un-written, not tombstoned"
        );
        assert_eq!(
            crate::capture_buffer::count_buffered(&pool).await.unwrap(),
            1,
            "the homeless claim waits in the queue"
        );
    }

    /// One section of a smart wiki's page, with a stand-in vector — the
    /// content itself is what the assertion is about, not its ranking.
    fn section(wiki_id: &str, ord: i64, text: &str) -> crate::sections::NewSection {
        crate::sections::NewSection {
            wiki_id: wiki_id.to_owned(),
            source_path: format!("wikis/{wiki_id}/index.md"),
            section_ord: ord,
            heading_path: None,
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
        }
    }

    /// A smart wiki holds its content in `wiki_sections`, which carries no
    /// sender and no ACL — so none of the three disposal modes applies to it,
    /// and deleting one drops its sections and its registry row outright.
    /// `Dissolve` is the sharp case: it is the mode that keeps **every** fact,
    /// and it must still take the sections, because they are not facts.
    ///
    /// The registry row is asserted separately and on purpose. It is what the
    /// readable-wiki filter is built from, so a row surviving the delete is
    /// not bookkeeping — it is recall still serving a wiki the operator just
    /// deleted, until the next safety-net tick.
    #[tokio::test]
    async fn deleting_a_smart_wiki_drops_its_sections_and_its_registry_row() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "progetto", "wiki-tech");
        let tree = WikiTree::open(dir.path()).unwrap();

        crate::sections::replace_page_sections(
            &pool,
            "wikis/progetto/index.md",
            &[
                section("progetto", 0, "What the shop does"),
                section("progetto", 1, "How orders are taken"),
            ],
        )
        .await
        .expect("seed sections");
        crate::sections::upsert_smart_wiki(
            &pool,
            &crate::sections::SmartWikiRow {
                wiki_id: "progetto".to_owned(),
                slug: "progetto".to_owned(),
                owner_id: "user:owner".parse().unwrap(),
                shared_with: Vec::new(),
                project_id: None,
                wiki_type: "wiki-tech".to_owned(),
                description: Some("The print shop.".to_owned()),
            },
        )
        .await
        .expect("seed registry");

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("progetto").unwrap(),
            &Principal::User("admin".to_owned()),
            DeletionMode::Dissolve,
        )
        .await
        .expect("delete");

        assert_eq!(report.sections_dropped, 2, "counted, never silent");
        assert!(
            crate::sections::find_wiki_sections(&pool, "progetto")
                .await
                .unwrap()
                .is_empty(),
            "a deleted smart wiki keeps no sections, in any disposal mode"
        );
        assert!(
            !crate::sections::list_smart_wikis(&pool)
                .await
                .unwrap()
                .iter()
                .any(|w| w.wiki_id == "progetto"),
            "the registry row goes at delete time, not at the next sweep"
        );
    }

    /// The standard wiki's twin of the case above. A page card is a
    /// projection of its page, and the card sweep only ever walks wikis still
    /// discovered on disk — so once the directory is in the trash nothing
    /// visits that wiki again and nothing collects its rows, embeddings
    /// included. The delete takes them, or nobody does.
    #[tokio::test]
    async fn deleting_a_standard_wiki_drops_its_page_cards() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let pool = crate::db::open_or_init(db_dir.path()).await.unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed(&tree, "dossier", "wiki-tech");
        let tree = WikiTree::open(dir.path()).unwrap();

        crate::page_card::upsert(
            &pool,
            &crate::page_card::NewPageCard {
                source_path: "wikis/dossier/cucina.md".to_owned(),
                wiki_id: "dossier".to_owned(),
                description: Some("What we cook.".to_owned()),
                keywords: vec!["cucina".to_owned()],
                style: Some(crate::wiki::PageStyle::Prosa),
                file_mtime_ms: Some(1_000),
                file_size: Some(42),
            },
        )
        .await
        .expect("seed card");

        let report = delete_wiki_subtree(
            &pool,
            &tree,
            &WikiId::parse("dossier").unwrap(),
            &Principal::User("admin".to_owned()),
            DeletionMode::Dissolve,
        )
        .await
        .expect("delete");

        assert_eq!(report.page_cards_dropped, 1, "counted, never silent");
        assert!(
            crate::page_card::list_for_wiki(&pool, "dossier")
                .await
                .unwrap()
                .is_empty(),
            "a deleted wiki keeps no page cards"
        );
    }
}
