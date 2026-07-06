// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hierarchical scope changes — `_internal.wiki_change_scope`.
//!
//! Moves a wiki (and its subtree) to a new parent in the tree while
//! keeping `wiki_id` stable per the
//! [memory model](../../../docs/concepts/memory-model.md)
//! invariant that ids never get rewritten on rename. Sub-wikis ride
//! along automatically — only the directly moved wiki's `_meta.md`
//! needs its `parent_wiki_id` rewritten, since descendants reference
//! parents by stable id and not by path.
//!
//! This is a composite operation (filesystem + DB), so it lives in
//! its own module rather than inside [`crate::wiki`] which is the
//! single owner of the on-disk tree and explicitly does not touch
//! the fact index.
//!
//! ## Scope of this operation
//!
//! - Refuse a **smart** source wiki: its wiki-level read audience
//!   derives from its position in the tree (the scope principal), so a
//!   re-parent would change effective read access on the next
//!   reindex/push — smart wikis are the consumer's to place.
//! - Move directory atomically via `fs::rename` (same filesystem).
//! - Rewrite `_meta.md.parent_wiki_id` on the moved wiki.
//! - Sync the old / new parent's `_meta.md.children` denormalised
//!   list.
//! - Rebase `fact_index.source_path` for every row inside the moved
//!   subtree.
//!
//! ## Deferred work
//!
//! - **Applicative WAL** wrapping the multi-step write. Today a
//!   crash between filesystem rename and DB rebase leaves the index
//!   stale; the next watcher reindex reconciles, and the dashboard
//!   chat replays cleanly on restart. The WAL lands together with the
//!   structure-proposal rollback driver.
//! - **Cross-link rewriting** of `[[file]]` references. Today
//!   [`crate::capture::wiki_link`] emits `[[wiki_id]]` / `[[wiki_id/page]]`
//!   form only, and `wiki_id` is stable across moves, so existing
//!   links never need rewriting. If a path-based link format ever
//!   ships, this module is the place to extend.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::fact_index;
use crate::types::WikiId;
use crate::wiki::{META_FILENAME, WikiChildEntry, WikiError, WikiMeta, WikiTree, atomic_write};

/// Outcome of [`wiki_change_scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeScopeOutcome {
    /// Stable id of the moved wiki (unchanged across the move).
    pub wiki_id: WikiId,
    /// Old directory relative to the workdir (e.g. `wikis/alice/acmecorp`).
    pub old_rel_dir: PathBuf,
    /// New directory relative to the workdir.
    pub new_rel_dir: PathBuf,
    /// New parent wiki id (`None` when the wiki was promoted to root).
    pub new_parent_wiki_id: Option<WikiId>,
    /// Number of `fact_index` rows whose `source_path` was rebased.
    pub facts_rebased: u64,
}

/// `_internal.wiki_change_scope` — move a wiki (and its subtree) to a
/// new parent while keeping its `wiki_id` stable.
///
/// `new_parent_id = None` promotes the wiki to a top-level root.
///
/// # Errors
///
/// - [`WikiError::WikiNotFound`] when source or new parent is unknown.
/// - [`WikiError::Io`] for filesystem failures (rename across
///   filesystems, target directory already exists, permission errors).
/// - [`WikiError::InvalidFrontmatter`] for cycles (moving a wiki under
///   itself or one of its descendants) and for a **smart** source wiki
///   (smart wikis are the consumer's; their derived wiki-level read
///   audience follows the tree position).
pub async fn wiki_change_scope(
    tree: &WikiTree,
    pool: &SqlitePool,
    source_id: &WikiId,
    new_parent_id: Option<&WikiId>,
) -> Result<ChangeScopeOutcome, WikiError> {
    let plan = plan_move(tree, source_id, new_parent_id)?;
    if plan.is_noop() {
        return Ok(plan.into_noop_outcome(source_id, new_parent_id));
    }
    if plan.new_abs_dir.exists() {
        return Err(WikiError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "target directory {} already exists",
                plan.new_abs_dir.display(),
            ),
        )));
    }

    move_directory(&plan.source_old_abs, &plan.new_abs_dir)?;
    rewrite_parent_wiki_id(&plan.new_abs_dir, new_parent_id.cloned())?;
    sync_parent_children(tree, &plan, source_id, new_parent_id)?;

    let old_prefix = format!(
        "{}/",
        plan.source_old_rel.to_string_lossy().replace('\\', "/"),
    );
    let new_rel = workdir_relative_to(tree.workdir(), &plan.new_abs_dir);
    let new_prefix = format!("{}/", new_rel.to_string_lossy().replace('\\', "/"));
    let facts_rebased = fact_index::rebase_source_path_prefix(pool, &old_prefix, &new_prefix)
        .await
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))?;

    tracing::info!(
        wiki_id = source_id.as_str(),
        old_rel_dir = %plan.source_old_rel.display(),
        new_rel_dir = %new_rel.display(),
        new_parent = new_parent_id.map(WikiId::as_str),
        facts_rebased,
        "scope: CHANGE_SCOPE done"
    );

    Ok(ChangeScopeOutcome {
        wiki_id: source_id.clone(),
        old_rel_dir: plan.source_old_rel,
        new_rel_dir: new_rel,
        new_parent_wiki_id: new_parent_id.cloned(),
        facts_rebased,
    })
}

/// Per-call scratch state precomputed once so the body of
/// [`wiki_change_scope`] reads top-to-bottom.
struct MovePlan {
    source_old_abs: PathBuf,
    source_old_rel: PathBuf,
    old_parent_id: Option<WikiId>,
    source_slug: String,
    source_wiki_type: String,
    new_abs_dir: PathBuf,
}

impl MovePlan {
    /// Plan would not change anything (the source already lives where
    /// the request would put it).
    fn is_noop(&self) -> bool {
        self.source_old_abs == self.new_abs_dir
    }

    fn into_noop_outcome(
        self,
        source_id: &WikiId,
        new_parent_id: Option<&WikiId>,
    ) -> ChangeScopeOutcome {
        ChangeScopeOutcome {
            wiki_id: source_id.clone(),
            old_rel_dir: self.source_old_rel.clone(),
            new_rel_dir: self.source_old_rel,
            new_parent_wiki_id: new_parent_id.cloned(),
            facts_rebased: 0,
        }
    }
}

fn plan_move(
    tree: &WikiTree,
    source_id: &WikiId,
    new_parent_id: Option<&WikiId>,
) -> Result<MovePlan, WikiError> {
    let source_handle = tree.locate(source_id)?;
    let source_old_abs = source_handle.abs_dir().to_path_buf();
    let source_old_rel = source_handle.rel_dir().to_path_buf();
    let old_parent_id = source_handle.meta().parent_wiki_id.clone();
    let source_slug = source_handle.meta().slug.as_str().to_owned();
    let source_wiki_type = source_handle.meta().wiki_type.clone();

    // A smart wiki's wiki-level read audience derives from its position in
    // the tree (the scope principal), so a re-parent would change effective
    // read access on the next reindex/push. Smart wikis are the consumer's —
    // refuse for every caller, not just the dashboard chat.
    if source_handle.meta().smart {
        return Err(WikiError::InvalidFrontmatter {
            path: source_old_abs,
            detail: format!(
                "wiki `{}` is smart — smart wikis are the consumer's, governed at the wiki \
                 level; a scope change would alter its derived read audience",
                source_id.as_str(),
            ),
        });
    }

    // A wiki's owning principal is **derived** from its path to the root
    // identity wiki, never declared — so a re-parent (including a
    // promote-to-root) never rewrites a principal. The old "refuse
    // promote-to-root while `acl_default = inherit`" guard is gone with the
    // `Inherit` variant.

    let new_abs_dir = if let Some(new_parent) = new_parent_id {
        if new_parent == source_id {
            return Err(WikiError::InvalidFrontmatter {
                path: source_old_abs,
                detail: "cannot move a wiki under itself".into(),
            });
        }
        let new_parent_handle = tree.locate(new_parent)?;
        if new_parent_handle.rel_dir().starts_with(&source_old_rel) {
            return Err(WikiError::InvalidFrontmatter {
                path: source_old_abs,
                detail: format!(
                    "cannot move wiki `{src}` under its descendant `{tgt}`",
                    src = source_id.as_str(),
                    tgt = new_parent.as_str(),
                ),
            });
        }
        new_parent_handle.abs_dir().join(&source_slug)
    } else {
        tree.wikis_dir().join(&source_slug)
    };

    Ok(MovePlan {
        source_old_abs,
        source_old_rel,
        old_parent_id,
        source_slug,
        source_wiki_type,
        new_abs_dir,
    })
}

fn move_directory(source_abs: &Path, target_abs: &Path) -> Result<(), WikiError> {
    if let Some(parent) = target_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(source_abs, target_abs)?;
    Ok(())
}

fn sync_parent_children(
    tree: &WikiTree,
    plan: &MovePlan,
    source_id: &WikiId,
    new_parent_id: Option<&WikiId>,
) -> Result<(), WikiError> {
    if let Some(old_parent) = &plan.old_parent_id {
        let h = tree.locate(old_parent)?;
        remove_child_from_parent_meta(h.abs_dir(), source_id)?;
    }
    if let Some(new_parent) = new_parent_id {
        let h = tree.locate(new_parent)?;
        let child = WikiChildEntry {
            wiki_id: source_id.as_str().to_owned(),
            slug: plan.source_slug.clone(),
            wiki_type: plan.source_wiki_type.clone(),
        };
        add_child_to_parent_meta(h.abs_dir(), child)?;
    }
    Ok(())
}

/// Open the wiki's `_meta.md`, set `parent_wiki_id` to `new_parent`
/// (or null for promote-to-root), and atomically rewrite the file.
/// Body content after the frontmatter is preserved verbatim.
fn rewrite_parent_wiki_id(abs_dir: &Path, new_parent: Option<WikiId>) -> Result<(), WikiError> {
    let meta_path = abs_dir.join(META_FILENAME);
    let raw = std::fs::read_to_string(&meta_path)?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw)?;
    meta.parent_wiki_id = new_parent;
    let rendered = meta
        .render(&body)
        .map_err(|e| WikiError::InvalidFrontmatter {
            path: meta_path.clone(),
            detail: format!("rendering rewritten meta: {e}"),
        })?;
    atomic_write(&meta_path, rendered.as_bytes())
}

fn remove_child_from_parent_meta(parent_abs: &Path, child_id: &WikiId) -> Result<(), WikiError> {
    let meta_path = parent_abs.join(META_FILENAME);
    let raw = std::fs::read_to_string(&meta_path)?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw)?;
    let before = meta.children.len();
    meta.children.retain(|c| c.wiki_id != child_id.as_str());
    if meta.children.len() == before {
        // Child wasn't listed — denormalised children may legitimately
        // be missing on legacy wikis. No-op keeps the call idempotent.
        return Ok(());
    }
    let rendered = meta
        .render(&body)
        .map_err(|e| WikiError::InvalidFrontmatter {
            path: meta_path.clone(),
            detail: format!("rendering parent meta after child removal: {e}"),
        })?;
    atomic_write(&meta_path, rendered.as_bytes())
}

fn add_child_to_parent_meta(parent_abs: &Path, child: WikiChildEntry) -> Result<(), WikiError> {
    let meta_path = parent_abs.join(META_FILENAME);
    let raw = std::fs::read_to_string(&meta_path)?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw)?;
    if meta.children.iter().any(|c| c.wiki_id == child.wiki_id) {
        return Ok(());
    }
    meta.children.push(child);
    let rendered = meta
        .render(&body)
        .map_err(|e| WikiError::InvalidFrontmatter {
            path: meta_path.clone(),
            detail: format!("rendering parent meta after child add: {e}"),
        })?;
    atomic_write(&meta_path, rendered.as_bytes())
}

/// `abs.strip_prefix(workdir)` returning a `PathBuf` (with a sane
/// fallback when the path is somehow not under the workdir — should
/// never happen for paths produced by this module).
fn workdir_relative_to(workdir: &Path, abs: &Path) -> PathBuf {
    abs.strip_prefix(workdir)
        .map_or_else(|_| abs.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
    use crate::embedder::{Embedder, FakeEmbedder};
    use crate::fact_index;
    use crate::types::{FactId, Principal, WikiId};
    use crate::wiki::{META_FILENAME, WikiTree};

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
        Arc::new(FakeEmbedder::new("fake-bge-m3", 4))
    }

    fn write_root(tree: &WikiTree, slug: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-user\nparent_wiki_id: null\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n",
        );
        std::fs::write(dir.join(META_FILENAME), meta).unwrap();
    }

    fn write_child(tree: &WikiTree, parent_slug: &str, parent_id: &str, slug: &str) {
        let dir = tree.wikis_dir().join(parent_slug).join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\nwiki_id: {parent_id}-{slug}\nwiki_type: wiki-cliente\nparent_wiki_id: {parent_id}\nslug: {slug}\ntitle: {slug}\nacl_default: inherit\n---\n",
        );
        std::fs::write(dir.join(META_FILENAME), meta).unwrap();
    }

    fn write_child_concrete_acl(
        tree: &WikiTree,
        parent_slug: &str,
        parent_id: &str,
        slug: &str,
        owner_user: &str,
    ) {
        let dir = tree.wikis_dir().join(parent_slug).join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\nwiki_id: {parent_id}-{slug}\nwiki_type: wiki-cliente\nparent_wiki_id: {parent_id}\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{owner_user}'\n---\n",
        );
        std::fs::write(dir.join(META_FILENAME), meta).unwrap();
    }

    async fn capture(
        pool: &SqlitePool,
        tree: &WikiTree,
        wiki: &str,
        page: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: format!("user:{}", wiki.split('-').next().unwrap_or(wiki))
                .parse::<Principal>()
                .unwrap(),
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
        let out = wiki_capture(tree, pool, embedder(), req).await.unwrap();
        match out.action {
            CaptureAction::Captured { .. } => out.fact_id,
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn change_scope_moves_root_under_existing_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_root(&tree, "bob");
        let pool = make_pool().await;
        let f1 = capture(&pool, &tree, "alice", "intro.md", "alice fact one").await;
        let f2 = capture(&pool, &tree, "alice", "intro.md", "alice fact two").await;

        let outcome = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice").unwrap(),
            Some(&WikiId::parse("bob").unwrap()),
        )
        .await
        .expect("scope change");
        assert_eq!(outcome.facts_rebased, 2);
        assert!(outcome.old_rel_dir.ends_with("wikis/alice"));
        assert!(outcome.new_rel_dir.ends_with("wikis/bob/alice"));

        // Directory has moved on disk.
        assert!(!tree.wikis_dir().join("alice").exists());
        assert!(tree.wikis_dir().join("bob/alice").exists());

        // The moved wiki's parent_wiki_id now points at bob.
        let moved_meta =
            std::fs::read_to_string(tree.wikis_dir().join("bob/alice").join(META_FILENAME))
                .unwrap();
        assert!(moved_meta.contains("parent_wiki_id: bob"));

        // Bob now lists alice as a child.
        let bob_meta =
            std::fs::read_to_string(tree.wikis_dir().join("bob").join(META_FILENAME)).unwrap();
        assert!(bob_meta.contains("wiki_id: alice"));

        // fact_index rows for alice point at the new path.
        for fid in [&f1, &f2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(row.source_path, "wikis/bob/alice/intro.md");
            // wiki_id stayed stable (the id-stability invariant).
            assert_eq!(row.wiki_id, "alice");
        }
    }

    #[tokio::test]
    async fn change_scope_promotes_subwiki_to_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_child_concrete_acl(&tree, "alice", "alice", "acmecorp", "alice");
        let pool = make_pool().await;
        // Capture a fact in the sub-wiki via wiki_capture so the
        // source_path is exactly what the production pipeline emits.
        let f = capture(&pool, &tree, "alice-acmecorp", "intro.md", "client fact").await;

        let outcome = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice-acmecorp").unwrap(),
            None,
        )
        .await
        .expect("scope change");
        assert_eq!(outcome.facts_rebased, 1);
        assert_eq!(outcome.new_parent_wiki_id, None);

        // Directory promoted to root.
        assert!(!tree.wikis_dir().join("alice/acmecorp").exists());
        assert!(tree.wikis_dir().join("acmecorp").exists());

        // The moved wiki's parent_wiki_id is now null.
        let moved_meta =
            std::fs::read_to_string(tree.wikis_dir().join("acmecorp").join(META_FILENAME)).unwrap();
        assert!(moved_meta.contains("parent_wiki_id: null"));

        let row = fact_index::find_by_id(&pool, &f).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/acmecorp/intro.md");
    }

    fn write_smart_root(tree: &WikiTree, slug: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-tech\nparent_wiki_id: null\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:alice'\nsmart: true\n---\n",
        );
        std::fs::write(dir.join(META_FILENAME), meta).unwrap();
    }

    /// A smart source wiki is refused: its wiki-level read audience derives
    /// from its position in the tree, so a re-parent would change effective
    /// read access on the next reindex/push.
    #[tokio::test]
    async fn change_scope_refuses_smart_source_wiki() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_smart_root(&tree, "proj");
        let pool = make_pool().await;
        let err = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("proj").unwrap(),
            Some(&WikiId::parse("alice").unwrap()),
        )
        .await
        .expect_err("a smart source must be refused");
        match err {
            WikiError::InvalidFrontmatter { detail, .. } => {
                assert!(detail.contains("smart"), "{detail}");
            },
            other => panic!("expected InvalidFrontmatter, got {other:?}"),
        }
        // Nothing moved on disk.
        assert!(tree.wikis_dir().join("proj").exists());
        assert!(!tree.wikis_dir().join("alice/proj").exists());
    }

    #[tokio::test]
    async fn change_scope_rejects_self_move() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        let pool = make_pool().await;
        let err = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice").unwrap(),
            Some(&WikiId::parse("alice").unwrap()),
        )
        .await
        .expect_err("must reject self move");
        assert!(matches!(err, WikiError::InvalidFrontmatter { .. }));
    }

    #[tokio::test]
    async fn change_scope_rejects_cycle_to_descendant() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_child(&tree, "alice", "alice", "acmecorp");
        let pool = make_pool().await;
        // Try to move alice under its own child alice-acmecorp.
        let err = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice").unwrap(),
            Some(&WikiId::parse("alice-acmecorp").unwrap()),
        )
        .await
        .expect_err("must reject cycle");
        match err {
            WikiError::InvalidFrontmatter { detail, .. } => {
                assert!(detail.contains("descendant"), "{detail}");
            },
            other => panic!("expected InvalidFrontmatter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn change_scope_rejects_when_target_dir_already_exists() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_root(&tree, "bob");
        // Pre-create a directory at the move's target slot so the
        // rename would clobber it.
        std::fs::create_dir_all(tree.wikis_dir().join("bob/alice")).unwrap();
        std::fs::write(tree.wikis_dir().join("bob/alice/decoy.md"), "hi").unwrap();
        let pool = make_pool().await;
        let err = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice").unwrap(),
            Some(&WikiId::parse("bob").unwrap()),
        )
        .await
        .expect_err("must refuse to overwrite");
        match err {
            WikiError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
            other => panic!("expected Io(AlreadyExists), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn change_scope_noop_when_already_under_target_parent() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        write_root(&tree, "alice");
        write_child(&tree, "alice", "alice", "acmecorp");
        let pool = make_pool().await;
        let outcome = wiki_change_scope(
            &tree,
            &pool,
            &WikiId::parse("alice-acmecorp").unwrap(),
            Some(&WikiId::parse("alice").unwrap()),
        )
        .await
        .expect("no-op succeeds");
        assert_eq!(outcome.facts_rebased, 0);
        assert!(outcome.old_rel_dir.ends_with("wikis/alice/acmecorp"));
        assert!(outcome.new_rel_dir.ends_with("wikis/alice/acmecorp"));
    }
}
