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
//!   home wiki — the `subject`/`allow`/`sender` ACL rides along untouched, so
//!   reading (per-fragment) is unchanged wherever it lands;
//! - when the sender is **gone** (no home wiki — a removed/never-enrolled
//!   principal) the **subject** is the fallback (subject == deleter → tombstone,
//!   else evacuate to the subject's wiki); when neither has a home wiki the fact
//!   is tombstoned (nobody to hand it to).
//!
//! The emptied page husk is dropped by the planner GC on the next compile.
//! Deleting a page is **admin authority** (structure is recall shape, not
//! access — the verb layer gates it); the per-fragment sender axis still
//! governs how each fact on the page is disposed. Smart wikis carry no per-fragment sender and are out of scope here
//! (their single proprietor decides — see
//! smart-wikis.md); this primitive is for
//! **standard** wikis.

use sqlx::SqlitePool;

use crate::capture_buffer;
use crate::fact_index::{self, FactIndexError};
use crate::promote::DirectPromoteError;
use crate::types::{Principal, WikiId};
use crate::wiki::{WikiError, WikiTree};

/// Failure of a page-level operation.
#[derive(Debug, thiserror::Error)]
pub enum PageError {
    /// Resolving the wiki or page in the tree failed.
    #[error(transparent)]
    Wiki(#[from] WikiError),
    /// A `fact_index` read or tombstone failed.
    #[error(transparent)]
    FactIndex(#[from] FactIndexError),
    /// Handing a foreign fact back to the capture buffer failed.
    #[error(transparent)]
    Buffer(#[from] crate::capture_buffer::CaptureBufferError),
    /// An evacuation (cross-wiki refile) failed.
    #[error(transparent)]
    Refile(#[from] DirectPromoteError),
    /// Resolving the owning group's member roster failed.
    #[error("page deletion db: {0}")]
    Db(#[from] sqlx::Error),
    /// [`DeletionMode::Dissolve`] reached the page verb. It is a whole-wiki
    /// gesture (see [`crate::wiki_delete::delete_wiki_subtree`]).
    #[error("dissolve is a whole-wiki mode; a page cannot be dissolved")]
    DissolveIsWholeWikiOnly,
}

/// What happened to the facts on a deleted page.
#[derive(Debug, Default, Clone)]
pub struct PageDeletionOutcome {
    /// Facts tombstoned (the deleter's own, or facts with no enrolled home).
    pub facts_tombstoned: u64,
    /// Foreign-authored facts evacuated to their sender's (or subject's) wiki.
    pub facts_evacuated: u64,
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
    /// **Dissolve**: destroy the *structure*, keep every fact. Nothing is
    /// tombstoned — each fact goes back into the capture buffer, so the next
    /// placement pass decides where it belongs, corpus-wide, instead of it
    /// inheriting the page it happened to sit on. Whole-wiki only (see
    /// [`crate::wiki_delete::delete_wiki_subtree`]).
    Dissolve,
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
/// Act-first: tombstones + evacuations apply immediately. A page with no
/// active facts is a no-op. `policy` may override the per-fact disposition
/// (the admin `TombstoneAll` mode, see [`DeletionPolicy`]).
///
/// # Errors
///
/// [`PageError`] on a wiki-resolution, index, or refile failure.
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

    let mut out = PageDeletionOutcome::default();
    let now = chrono::Utc::now().to_rfc3339();
    for row in &rows {
        // The principal responsible for the fact: its sender (provenance) when
        // materialized, else its subject — the sender-gone fallback.
        // Admin "delete all" collapses every fact onto the tombstone arm.
        let responsible = row.sender_id.as_ref().unwrap_or(&row.subject_id);
        let action = match policy.mode {
            DeletionMode::TombstoneAll => Action::Tombstone,
            DeletionMode::SenderKeyed => decide(responsible, &row.subject_id, deleter, tree),
            // Dissolving is a whole-wiki gesture: it re-opens the placement
            // of everything it frees so the Cartografo redistributes it.
            // On a single page that is not a deletion at all — it is what
            // the REM split/merge passes already do, page by page — so the
            // page verb refuses the mode rather than half-honouring it.
            DeletionMode::Dissolve => {
                return Err(PageError::DissolveIsWholeWikiOnly);
            },
        };
        match action {
            Action::Tombstone => {
                fact_index::mark_forgotten(pool, &row.fact_id, reason).await?;
                out.facts_tombstoned += 1;
            },
            Action::Evacuate => {
                capture_buffer::rebuffer_fact(pool, &row.fact_id, &now).await?;
                out.facts_evacuated += 1;
            },
        }
    }
    Ok(out)
}

pub(crate) enum Action {
    Tombstone,
    /// Hand the fact back: it goes into the capture buffer, and the next
    /// placement pass writes it wherever its subject lives now.
    Evacuate,
}

/// The per-fact decision (module docs): tombstone the deleter's own
/// contribution; evacuate a foreign one to its sender's home wiki; fall back to
/// the subject when the sender has no home wiki; tombstone when neither does.
///
/// Shared with [`crate::wiki_delete::delete_wiki_subtree`], whose `SenderKeyed`
/// whole-wiki evacuation partitions every fact in the subtree by exactly this
/// rule. The caller computes `responsible` = the fact's `sender` (provenance)
/// when present, else its `subject` (the sender-gone fallback).
pub(crate) fn decide(
    responsible: &Principal,
    subject: &Principal,
    deleter: &Principal,
    tree: &WikiTree,
) -> Action {
    if responsible == deleter {
        return Action::Tombstone;
    }
    if existing_home_wiki(responsible, tree).is_some() {
        return Action::Evacuate;
    }
    // Sender gone (no home wiki) → fall back to the subject axis.
    if subject == deleter {
        return Action::Tombstone;
    }
    if existing_home_wiki(subject, tree).is_some() {
        return Action::Evacuate;
    }
    Action::Tombstone
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
    fn decide_partitions_by_sender_with_subject_fallback() {
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
        // foreign sender with a home wiki → hand it back, intact.
        assert!(matches!(
            decide(&morgana, &franz, &franz, &tree),
            Action::Evacuate
        ));
        // sender gone (no wiki), subject == deleter → tombstone.
        assert!(matches!(
            decide(&ghost, &franz, &franz, &tree),
            Action::Tombstone
        ));
        // sender gone, subject has a wiki ≠ deleter → hand it back.
        assert!(matches!(
            decide(&ghost, &morgana, &franz, &tree),
            Action::Evacuate
        ));
        // neither sender nor subject has a home wiki → tombstone (nobody to hand
        // it to).
        assert!(matches!(
            decide(&ghost, &ghost2, &franz, &tree),
            Action::Tombstone
        ));
    }
}
