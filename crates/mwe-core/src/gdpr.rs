// SPDX-License-Identifier: AGPL-3.0-or-later
//! The two things a person may ask of a memory that holds them: a copy of
//! what it knows about them ([`export_user`]), and its removal
//! ([`forget_user`]).
//!
//! ## Forgetting is not deleting every sentence with their name in it
//!
//! Three kinds of sentence in a memory touch one person, and only the first
//! is theirs:
//!
//! - **What they said about themselves** — their wiki, and every fact whose
//!   subject and sender are both them. It goes, and unlike every other
//!   retirement it leaves no tombstone: a retired row keeps its claim text,
//!   and the claim text is the thing being erased.
//! - **What somebody else said about them** — *«alice ieri ha fatto un ottimo
//!   lavoro con la presentazione per il cliente»*. That is **the speaker's**
//!   memory of their own life, and erasing alice must not erase it. So the
//!   fact changes hands: the sender becomes the subject that answers for it,
//!   and alice survives on it as a plain name in `subject_external` — which
//!   grants nothing and addresses nobody, so the sentence keeps its meaning
//!   without keeping an identity. Filed in her wiki, it moves into theirs.
//! - **What they said about somebody else** — that is the other person's
//!   memory. It stays exactly where it is, and only the name of who said it
//!   goes, replaced by [`removed_sender`].
//!
//! Founder, 2026-09-06: *«non è giusto che la memoria di un utente sia
//! eliminata se riguarda i fatti che come soggetto hanno un utente che si
//! elimina … le memorie con subject alice ma con sender diverso da alice
//! andranno a finire con `subject_external` alice»*.
//!
//! A memory whose keeper has *also* been forgotten has nobody left to keep
//! it: a fact about this person whose author is already the tombstone is
//! destroyed rather than handed to an identity nobody holds.
//!
//! **The rules page is the exception, and it is the one place it has to be.**
//! A behaviour rule is a directive, not a memory: *"write to her in
//! English"*, handed to whoever wrote it, becomes a directive about **them**.
//! So a rule whose subject is the forgotten person is deleted, whoever wrote
//! it. The test of a rule is where it lives ([`crate::wiki::is_rules_page`]),
//! which is the same test the channel that serves rules applies.
//!
//! ## No copy is kept, and the one the pass cannot reach
//!
//! An admin wiki delete moves the directory into `<workdir>/trash/` and lets
//! `retention.trash_days` finish the job, because the operator may have
//! deleted the wrong wiki. A person's erasure has no such window: the subtree
//! is erased in place ([`crate::wiki_delete::HuskFate::Erase`]), and any
//! subtree of theirs already sitting in the trash from an earlier delete is
//! erased with it.
//!
//! The **training spool** is the other copy on disk, and it goes whole. With
//! `training_spool` on, every model call appends the entire prompt to
//! `<workdir>/training-spool/`, and a prompt carries the recalled memory
//! verbatim — so those files hold the person, under no subject column of
//! their own. They are emptied rather than filtered, and
//! [`crate::training_spool::erase_all`] carries why a per-line filter would
//! be a false claim.
//!
//! A **snapshot** is the copy this pass cannot reach: a sealed archive of
//! the workdir as it was, which restoring brings back whole, person
//! included. The confirmation page says so and points at the backup console
//! — what to do with an old snapshot is the operator's decision, and the
//! engine is not in a position to take it for them.
//!
//! ## The id does not come back
//!
//! What survives an erasure still names the person: a fact handed to another
//! speaker keeps their name in `subject_external`, and a page somebody else
//! wrote still says it. Hand the id to a new account and every one of those
//! sentences reads as being about whoever holds it now — the erasure would
//! have moved the material instead of removing it. So the id is written to
//! `forgotten_user_ids` (migration `0077`) and
//! [`crate::enrollment::reject_if_forgotten`] refuses it at every point a
//! user is created. That list is the one thing an erasure keeps, and it
//! keeps it in order to honour itself.
//!
//! ## Everything the identity is written on, in one place
//!
//! The rest of this module is a sweep across the tables that carry a user id,
//! and it is deliberately one list rather than a method per table: what makes
//! an erasure correct is that nothing was forgotten, and that is a property a
//! reader has to be able to check by reading down a page. Each table is in
//! exactly one of three groups:
//!
//! - **[`PERSONAL_ROWS`]** — the rows *are* the person: the queries they ran,
//!   the notices addressed to them, the documents they handed the engine.
//!   Deleted.
//! - **[`AUDIT_COLUMNS`]** — the record that something happened, which
//!   survives; the name of who did it, which does not. Re-stamped with
//!   [`removed_sender`].
//! - **[`MEMBERSHIP_LISTS`]** — the JSON arrays that grant something by
//!   naming a principal: group membership, a consumer's act-as grant, a
//!   wiki's sharing list, an audit snapshot's allow list. Their id is struck
//!   out.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::enrollment;
use crate::export;
use crate::fact_index::{self, FactIndexRow};
use crate::media;
use crate::page::DeletionMode;
use crate::promote;
use crate::types::{FactId, Principal, WikiId};
use crate::wiki::{self, WikiTree};
use crate::wiki_delete::{self, HuskFate};

/// `deleted_reason` stamped on every fact a person's erasure tombstones.
pub const FORGET_REASON: &str = "gdpr_erasure";

/// The user id a forgotten author's facts carry in place of their own.
///
/// The leading underscore is what makes it unclaimable rather than a
/// convention: [`crate::enrollment::is_valid_user_id`] refuses `_`, so nobody
/// can ever enrol as this principal and inherit a dead person's
/// contributions, and [`WikiId::parse`] refuses it too, so it can never
/// resolve to a home wiki. It matches no reader, so it grants no read
/// ([`crate::acl::can_read`]) and no deletion ([`crate::acl::can_delete`]) —
/// it fills the provenance slot and does nothing else.
pub const REMOVED_USER_ID: &str = "_removed";

/// The principal [`REMOVED_USER_ID`] names.
#[must_use]
pub fn removed_sender() -> Principal {
    Principal::User(REMOVED_USER_ID.to_owned())
}

/// Whether `principal` is the forgotten-author tombstone.
///
/// Asked wherever a principal is presented as a **person** rather than
/// evaluated as a permission: the electorate a forget vote is put to
/// ([`crate::acl::audience`]) and the audience the Cronista is shown beside a
/// fact. The permission checks need no such test — the id matches no reader,
/// so it grants nothing on its own.
#[must_use]
pub fn is_removed(principal: &Principal) -> bool {
    matches!(principal, Principal::User(id) if id == REMOVED_USER_ID)
}

/// How a table spells a principal: bare `alice`, or wire `user:alice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdForm {
    /// The bare user id, as a session carries it.
    Bare,
    /// The `Principal` wire form.
    Wire,
}

/// Rows that **are** the person rather than a record that something
/// happened: deleted outright.
const PERSONAL_ROWS: &[(&str, &str, IdForm)] = &[
    // What they searched for, and the turns behind it.
    ("recall_traces", "sender_id", IdForm::Bare),
    ("recall_log", "sender_id", IdForm::Bare),
    ("recall_misses", "sender_id", IdForm::Bare),
    // A wiki-admin lease is a live claim on a wiki; theirs expires with them.
    ("wiki_admin_leases", "sender_id", IdForm::Bare),
    // Briefing items carry the prose they wrote.
    ("wiki_briefing_items", "source_ref", IdForm::Wire),
    // A document job carries the whole document they handed in, segment by
    // segment (`document_job_segments` goes with it, see `drop_personal_rows`).
    ("document_jobs", "subject_id", IdForm::Wire),
];

/// The audit trail: what happened stays, who did it does not.
const AUDIT_COLUMNS: &[(&str, &str, IdForm)] = &[
    ("tool_executions", "sender_id", IdForm::Bare),
    ("wiki_admin_op_log", "sender_id", IdForm::Bare),
    ("archive_proposals", "decided_by", IdForm::Bare),
    ("structure_proposals", "applied_by", IdForm::Bare),
    ("disclosure_audit", "actor_id", IdForm::Bare),
    ("disclosure_audit", "prev_subject_id", IdForm::Wire),
    ("disclosure_audit", "new_subject_id", IdForm::Wire),
    ("disclosure_audit", "prev_sender_id", IdForm::Wire),
    ("disclosure_audit", "new_sender_id", IdForm::Wire),
    // A document job somebody else is the subject of, submitted by them.
    ("document_jobs", "sender_id", IdForm::Wire),
];

/// The JSON arrays that grant something by naming a principal.
const MEMBERSHIP_LISTS: &[(&str, &str, IdForm)] = &[
    ("enrollment_groups", "members", IdForm::Bare),
    ("consumer_delegations", "allowed_sender_ids", IdForm::Bare),
    ("smart_wikis", "shared_with", IdForm::Wire),
    ("media_catalog", "allow_ids", IdForm::Wire),
    ("capture_buffer", "allow_ids", IdForm::Wire),
    ("disclosure_audit", "prev_allow_ids", IdForm::Wire),
    ("disclosure_audit", "new_allow_ids", IdForm::Wire),
];

/// Failure modes of the two person-scoped movements.
#[derive(Debug, thiserror::Error)]
pub enum GdprError {
    /// Underlying `SQLite` error.
    #[error("gdpr db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON (de)serialisation failure.
    #[error("gdpr json: {0}")]
    Json(#[from] serde_json::Error),
    /// `fact_index` read/write failure.
    #[error("gdpr fact index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),
    /// Capture-buffer hand-back failure.
    #[error("gdpr capture buffer: {0}")]
    Buffer(#[from] crate::capture_buffer::CaptureBufferError),
    /// `media_catalog` / blob-store failure.
    #[error("gdpr media: {0}")]
    Media(#[from] media::MediaError),
    /// Tree traversal / page read failure.
    #[error("gdpr wiki: {0}")]
    Wiki(#[from] wiki::WikiError),
    /// The erasure of the person's own wiki subtree failed.
    #[error("gdpr wiki delete: {0}")]
    WikiDelete(#[from] wiki_delete::WikiDeleteError),
    /// Building the person archive failed.
    #[error("gdpr export: {0}")]
    Export(#[from] export::ExportError),
    /// Archive / blob-store I/O failure.
    #[error("gdpr io: {0}")]
    Io(#[from] std::io::Error),
    /// The target is the deployment admin. Removing them leaves nobody who
    /// can operate the deployment, so it is refused here as it is on the
    /// user list.
    #[error("refusing to forget the deployment admin {0}")]
    IsDeploymentAdmin(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, GdprError>;

/// What one [`forget_user`] did, category by category — what the operator is
/// shown, and what the log line carries.
#[derive(Debug, Clone, Default)]
pub struct ForgetReport {
    /// The person who was forgotten.
    pub user_id: String,
    /// Facts somebody else told about them: handed to that speaker, carrying
    /// their name as `subject_external`.
    pub facts_handed_over: u64,
    /// Of those, the ones filed in the person's own wiki, which moved into
    /// the new subject's wiki.
    pub facts_moved: u64,
    /// Facts that had to leave the erased wiki and had no destination wiki
    /// to go to: freed into the capture buffer, where the next placement
    /// pass decides where they belong. Nothing is destroyed here.
    pub facts_unplaced: u64,
    /// Facts destroyed: what the person said about themselves, and every
    /// behaviour rule about them.
    pub facts_tombstoned: u64,
    /// Of those, the ones whose prose could not be taken off its page, so
    /// the row was left retired rather than deleted. Non-zero means a page
    /// somewhere still carries the sentence and wants an operator's look.
    pub facts_left_as_tombstone: u64,
    /// Facts about somebody else that they authored: kept where they are,
    /// with the author's name replaced by [`removed_sender`].
    pub facts_disowned: u64,
    /// Other people's facts that named them in the read-extension list.
    pub allow_lists_pruned: u64,
    /// Waiting captures somebody else made about them, handed over the same
    /// way as facts.
    pub captures_handed_over: u64,
    /// Waiting captures destroyed (their own about themselves).
    pub captures_dropped: u64,
    /// Media items they uploaded, removed from the catalog.
    pub media_removed: u64,
    /// Of those, the blobs erased from the store — one short of
    /// `media_removed` for every file somebody else had uploaded too.
    pub media_blobs_removed: u64,
    /// Notices and proposals addressed to them.
    pub notices_removed: u64,
    /// Rows deleted across [`PERSONAL_ROWS`].
    pub personal_rows_removed: u64,
    /// Rows re-stamped across [`AUDIT_COLUMNS`].
    pub audit_rows_anonymised: u64,
    /// Rows amended across [`MEMBERSHIP_LISTS`].
    pub lists_amended: u64,
    /// Consumer registrations bound to the identity, dismantled with it.
    pub consumers_dismantled: Vec<String>,
    /// Web-agent OAuth rows removed — the codes and refresh tokens that
    /// would otherwise keep minting access tokens for a sender who is gone.
    pub oauth_rows_removed: u64,
    /// Wikis erased — their identity wiki and everything under it.
    pub wikis_erased: usize,
    /// Subtrees of theirs erased out of `<workdir>/trash/`.
    pub trash_dirs_erased: usize,
    /// Training-spool files emptied. The spool records whole prompts, and a
    /// prompt carries the recalled memory verbatim, so it holds them —
    /// under no subject column of its own, which is why the whole spool
    /// goes ([`crate::training_spool::erase_all`]).
    pub training_spool_files_emptied: usize,
    /// Smart wikis outside their own subtree that still name them as owner.
    /// Left standing on purpose — a wiki somebody else may be reading is not
    /// this pass's to destroy — and reported so the operator can decide. No
    /// enrolled principal matches their scope while the id stays unclaimed.
    pub orphan_smart_wikis: Vec<String>,
}

/// Forget one person: everything the memory holds *of* them goes, everything
/// it holds *for somebody else* changes hands. See the module docs for the
/// three kinds of sentence and what happens to each.
///
/// Returns `Ok(None)` when no such user is enrolled.
///
/// The order is load-bearing. The fact passes run while the person's pages
/// are still on disk, so a fact leaving for somebody else's wiki can be read
/// out of the page it sits on; the enrollment row goes next, which is what
/// turns their identity wiki into an orphan the subtree delete will accept;
/// the directories go last.
///
/// # Errors
///
/// [`GdprError::IsDeploymentAdmin`] for the deployment admin; otherwise as
/// the engine layer the step failed in.
#[allow(
    clippy::too_many_lines,
    reason = "one linear erasure; splitting it hides the order the guarantees depend on"
)]
pub async fn forget_user(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    user_id: &str,
) -> Result<Option<ForgetReport>> {
    let is_admin: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    let Some(is_admin) = is_admin else {
        return Ok(None);
    };
    if is_admin != 0 {
        return Err(GdprError::IsDeploymentAdmin(user_id.to_owned()));
    }

    let gone = Principal::User(user_id.to_owned());
    let tombstone = removed_sender();
    let mut report = ForgetReport {
        user_id: user_id.to_owned(),
        ..ForgetReport::default()
    };

    // Their own wikis — the identity wiki and everything nested under it,
    // which is where a web-agent connection's dedicated wiki lives too.
    let root_wiki = WikiId::parse(user_id)
        .ok()
        .filter(|id| tree.locate(id).is_ok());
    let hers: BTreeSet<String> = root_wiki.as_ref().map_or_else(BTreeSet::new, |root| {
        wiki_delete::collect_subtree(tree, root).map_or_else(
            |_| BTreeSet::new(),
            |subtree| {
                subtree
                    .into_iter()
                    .map(|d| d.meta.wiki_id.as_str().to_owned())
                    .collect()
            },
        )
    });

    // 1 — every fact about them, sorted by [`fate_of`].
    for row in fact_index::find_active_by_subject(pool, &gone).await? {
        match fate_of(&row, &gone) {
            FactFate::HandOver(keeper) => {
                fact_index::retarget_subject(pool, &row.fact_id, &keeper, user_id).await?;
                report.facts_handed_over += 1;
            },
            FactFate::Destroy => {
                tombstone_fact(pool, tree, &embedder, &row, &hers, &mut report).await?;
            },
            // A fact reached through the subject axis is about them, so it
            // is one of the two above.
            FactFate::Disown | FactFate::Untouched => {},
        }
    }

    // 2 — what they said about other people stays where it is; the name of
    // who said it does not. Runs after step 1, so a fact just handed over
    // (whose sender is the new subject) is never touched here.
    report.facts_disowned = fact_index::replace_sender(pool, &gone, &tombstone).await?;

    // 3 — their name off other people's read-extension lists.
    report.allow_lists_pruned = fact_index::strike_from_allow(pool, &gone).await?;

    // 4 — whatever is still alive in their wikis has to leave before the
    // wikis do.
    for wiki_id in &hers {
        for row in fact_index::find_active_in_wiki(pool, wiki_id).await? {
            relocate(pool, tree, &row, &hers, &mut report).await?;
        }
    }

    // 5 — the same three rules over the claims still waiting in the queue,
    // which have no page and so never move.
    forget_captures(pool, user_id, &gone, &tombstone, &mut report).await?;

    // 6 — the media they uploaded, and the blobs nobody else addresses.
    for row in media::find_by_subject(pool, &gone).await? {
        let removal = media::remove(pool, tree.workdir(), &row.catalog_id).await?;
        report.media_removed += u64::from(removal.row_removed);
        report.media_blobs_removed += u64::from(removal.blob_removed);
    }

    // 7 — the tables that carry a user id, in the three groups the module
    // docs set out.
    report.personal_rows_removed = drop_personal_rows(pool, user_id).await?;
    report.notices_removed = drop_notices(pool, &gone).await?;
    report.audit_rows_anonymised = anonymise_audit(pool, user_id).await?;
    report.lists_amended = strike_from_lists(pool, user_id, &gone).await?;
    report.orphan_smart_wikis = orphan_smart_wikis(pool, &gone, &hers).await?;

    // 8 — the training spool, which is on disk and not in the database. It
    // records whole prompts, and a prompt carries the recalled memory
    // verbatim, so it holds what every step above has been removing.
    //
    // Before the enrollment row goes, and so is the id below, because
    // [`enrollment::remove_user`] is the point of no return for a **retry**:
    // once that row is gone `forget_user` answers `Ok(None)` and there is no
    // second attempt. Anything that can fail and must not be silently
    // skipped therefore runs while the person is still enrolled.
    report.training_spool_files_emptied = crate::training_spool::erase_all(tree.workdir())?;

    // 9 — the id is spent. Recorded while the row still exists, which is
    // safe in the other direction too: the list gates the *creation* of a
    // user and nothing consults it here, so an entry written before a run
    // that then fails blocks nothing, and the insert is idempotent.
    enrollment::record_forgotten_id(pool, user_id).await?;

    // 10 — the enrollment row, the consumers bound to the identity, the
    // OAuth artefacts and the recent window. This is also what turns their
    // identity wiki into an orphan, which is the only shape the subtree
    // delete accepts.
    if let Some(removal) = enrollment::remove_user(pool, user_id).await? {
        report.consumers_dismantled = removal.consumers_dismantled;
        report.oauth_rows_removed = removal.oauth_rows_removed;
    }

    // 11 — their wikis, erased rather than trashed. By now every fact that
    // was in them has left: handed over and refiled, freed into the queue,
    // or destroyed. `TombstoneAll` is the net under that — it counts
    // anything that somehow stayed, and on a healthy run it counts nothing.
    if let Some(root) = root_wiki {
        let deleted = wiki_delete::delete_wiki_subtree(
            pool,
            tree,
            &root,
            &gone,
            DeletionMode::TombstoneAll,
            HuskFate::Erase,
        )
        .await?;
        report.wikis_erased = deleted.wikis_removed;
        report.facts_tombstoned += deleted.facts_tombstoned;
    }

    // 12 — and whatever an earlier wiki delete left of theirs in the trash.
    report.trash_dirs_erased = erase_trashed_subtrees(tree, user_id);

    tracing::info!(
        user = user_id,
        handed_over = report.facts_handed_over,
        moved = report.facts_moved,
        unplaced = report.facts_unplaced,
        tombstoned = report.facts_tombstoned,
        left_as_tombstone = report.facts_left_as_tombstone,
        disowned = report.facts_disowned,
        allow_lists = report.allow_lists_pruned,
        captures_handed_over = report.captures_handed_over,
        captures_dropped = report.captures_dropped,
        media = report.media_removed,
        media_blobs = report.media_blobs_removed,
        notices = report.notices_removed,
        personal_rows = report.personal_rows_removed,
        audit_rows = report.audit_rows_anonymised,
        lists_amended = report.lists_amended,
        wikis = report.wikis_erased,
        trash_dirs = report.trash_dirs_erased,
        training_spool_files = report.training_spool_files_emptied,
        consumers = ?report.consumers_dismantled,
        oauth_rows = report.oauth_rows_removed,
        orphan_smart_wikis = ?report.orphan_smart_wikis,
        "gdpr: person forgotten"
    );
    Ok(Some(report))
}

/// What forgetting a person does to one fact — the whole judgement, in one
/// place, so the summary the operator reads before confirming and the pass
/// that runs afterwards cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FactFate {
    /// Theirs about themselves, or a behaviour rule about them. Tombstoned.
    Destroy,
    /// Somebody else's memory of them. The named principal becomes its
    /// subject and their name moves into `subject_external`.
    HandOver(Principal),
    /// Their memory of somebody else. Stays where it is; the author's name
    /// is replaced.
    Disown,
    /// Nothing to do with them.
    Untouched,
}

/// Sort one fact into its [`FactFate`].
fn fate_of(row: &FactIndexRow, gone: &Principal) -> FactFate {
    if row.subject_id == *gone {
        // A behaviour rule is a directive, not a memory: handed to whoever
        // wrote it, "write to her in English" becomes a directive about
        // them. See the module docs.
        if wiki::is_rules_page(&row.source_path) {
            return FactFate::Destroy;
        }
        // A keeper has to be somebody. An author already replaced by the
        // tombstone is nobody, so a fact about this person that only such an
        // author ever held has nobody left to keep it.
        return row
            .sender_id
            .as_ref()
            .filter(|s| **s != *gone && !is_removed(s))
            .map_or(FactFate::Destroy, |keeper| {
                FactFate::HandOver(keeper.clone())
            });
    }
    if row.sender_id.as_ref() == Some(gone) {
        return FactFate::Disown;
    }
    FactFate::Untouched
}

/// What a [`forget_user`] is about to do, counted before it does it — the
/// numbers the confirmation page shows.
#[derive(Debug, Default, Clone)]
pub struct ForgetPreview {
    /// Facts that will be destroyed: theirs about themselves, and the
    /// behaviour rules about them.
    pub facts_destroyed: u64,
    /// Facts that will change hands, keeping their name as a plain
    /// `subject_external`.
    pub facts_handed_over: u64,
    /// Facts of theirs about other people that stay where they are, with the
    /// author's name replaced.
    pub facts_disowned: u64,
    /// Media items they uploaded, which go.
    pub media: u64,
    /// Wikis that will be erased — their own and everything under it.
    pub wikis: usize,
    /// Training-spool files on disk, all of which will be emptied.
    pub training_spool_files: usize,
}

/// Count what [`forget_user`] would do, without doing any of it.
///
/// Returns `Ok(None)` when no such user is enrolled.
///
/// # Errors
///
/// As the engine layer the read failed in.
pub async fn forget_preview(
    pool: &SqlitePool,
    tree: &WikiTree,
    user_id: &str,
) -> Result<Option<ForgetPreview>> {
    let enrolled: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    if enrolled.is_none() {
        return Ok(None);
    }
    let gone = Principal::User(user_id.to_owned());
    let mut preview = ForgetPreview::default();
    for row in fact_index::find_active_by_subject(pool, &gone).await? {
        match fate_of(&row, &gone) {
            FactFate::Destroy => preview.facts_destroyed += 1,
            FactFate::HandOver(_) => preview.facts_handed_over += 1,
            FactFate::Disown | FactFate::Untouched => {},
        }
    }
    for row in fact_index::find_active_by_sender(pool, &gone).await? {
        if fate_of(&row, &gone) == FactFate::Disown {
            preview.facts_disowned += 1;
        }
    }
    preview.media = media::find_by_subject(pool, &gone).await?.len() as u64;
    preview.wikis = WikiId::parse(user_id)
        .ok()
        .filter(|id| tree.locate(id).is_ok())
        .and_then(|id| wiki_delete::collect_subtree(tree, &id).ok())
        .map_or(0, |subtree| subtree.len());
    preview.training_spool_files = crate::training_spool::file_count(tree.workdir())?;
    Ok(Some(preview))
}

/// Destroy one fact: its prose, then its row.
///
/// An ordinary retirement leaves a tombstone, which keeps the claim text.
/// Here the claim text is the thing being erased, so the row goes too
/// ([`fact_index::erase`]) — but only once nothing on disk still shows it:
/// the region is excised first, and for a fact inside the person's own wikis
/// there is nothing to excise because the whole directory is erased a few
/// steps later.
///
/// When the prose could **not** be taken off the page, the row stays retired
/// instead. That is deliberate and it is the lesser residue: a tombstoned row
/// is invisible to recall and is what the hygiene sweep keys on to finish the
/// job, whereas a row-less marker leaves the sentence readable on the page
/// with nothing left to find it by. The caller counts these so the operator
/// is told.
async fn tombstone_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    row: &FactIndexRow,
    hers: &BTreeSet<String>,
    report: &mut ForgetReport,
) -> Result<()> {
    fact_index::mark_forgotten(pool, &row.fact_id, FORGET_REASON).await?;
    report.facts_tombstoned += 1;
    let prose_gone = if hers.contains(&row.wiki_id) {
        true
    } else {
        match crate::reindex::strip_fact_region(pool, tree, Arc::clone(embedder), &row.fact_id)
            .await
        {
            Ok(stripped) => stripped,
            Err(e) => {
                tracing::warn!(
                    fact_id = row.fact_id.as_str(),
                    error = %e,
                    "gdpr: excising the fact's prose failed"
                );
                false
            },
        }
    };
    if prose_gone {
        fact_index::erase(pool, &row.fact_id).await?;
    } else {
        report.facts_left_as_tombstone += 1;
        tracing::warn!(
            fact_id = row.fact_id.as_str(),
            source_path = %row.source_path,
            "gdpr: the prose is still on the page, so the row stays retired \
             rather than deleted — the hygiene sweep finishes it, and this page \
             is worth an operator's look"
        );
    }
    Ok(())
}

/// Move one surviving fact out of a wiki that is about to be erased.
///
/// It goes to the home wiki of the principal that now answers for it, onto
/// the page named after what the fact is **about** — its `subject_external`
/// when it has one, its current page name when it does not. That single rule
/// files a fact handed to somebody else on a page bearing the forgotten
/// person's name, and leaves everything else on the page it already had.
///
/// A fact whose subject has no home wiki to go to (`global`, or a principal
/// whose wiki is itself being erased) is freed into the capture buffer
/// instead, where the next placement pass decides where it belongs — the
/// same hand-back a wiki dissolve performs, and it destroys nothing.
async fn relocate(
    pool: &SqlitePool,
    tree: &WikiTree,
    row: &FactIndexRow,
    hers: &BTreeSet<String>,
    report: &mut ForgetReport,
) -> Result<()> {
    let source = WikiId::parse(&row.wiki_id)
        .ok()
        .and_then(|id| tree.locate(&id).ok());
    let (Some(dest), Some(source)) = (destination_wiki(tree, &row.subject_id, hers), source) else {
        rebuffer(pool, &row.fact_id).await?;
        report.facts_unplaced += 1;
        return Ok(());
    };
    let rel_dir = source.rel_dir().to_string_lossy().replace('\\', "/");
    let source_page = row
        .source_path
        .strip_prefix(&format!("{rel_dir}/"))
        .unwrap_or(&row.source_path)
        .to_owned();
    let dest_page = destination_page(row, &source_page);
    match promote::refile_fact_across_wikis(
        pool,
        tree,
        &row.fact_id,
        &row.wiki_id,
        &source_page,
        dest.as_str(),
        &dest_page,
    )
    .await
    {
        Ok(()) => report.facts_moved += 1,
        Err(e) => {
            // The refile refused (a marker that is not on the page, a path
            // the destination will not take). The fact still has to leave
            // the wiki, and the queue takes a claim with no destination.
            tracing::warn!(
                fact_id = row.fact_id.as_str(),
                dest = dest.as_str(),
                error = %e,
                "gdpr: refile refused — the fact goes back to the queue instead"
            );
            rebuffer(pool, &row.fact_id).await?;
            report.facts_unplaced += 1;
        },
    }
    Ok(())
}

/// Hand one fact back to the capture buffer, carrying its axes untouched.
async fn rebuffer(pool: &SqlitePool, fact_id: &FactId) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    crate::capture_buffer::rebuffer_fact(pool, fact_id, &now).await?;
    Ok(())
}

/// The wiki a fact filed under `subject` should move to: that principal's
/// home wiki, when it exists, is standard, and is not itself being erased.
fn destination_wiki(
    tree: &WikiTree,
    subject: &Principal,
    hers: &BTreeSet<String>,
) -> Option<WikiId> {
    let id = match subject {
        Principal::User(id) => id.clone(),
        Principal::Group(id) if id != "global" => id.clone(),
        Principal::Group(_) => return None,
    };
    if hers.contains(&id) {
        return None;
    }
    let wiki_id = WikiId::parse(&id).ok()?;
    let handle = tree.locate(&wiki_id).ok()?;
    // A smart wiki has one writer and it is not this pass: its content lives
    // in sections, not in marked regions.
    (!handle.meta().smart).then_some(wiki_id)
}

/// The page a relocated fact lands on: named after what the fact is about
/// when it says so, otherwise the page name it already had.
fn destination_page(row: &FactIndexRow, source_page: &str) -> String {
    row.subject_external
        .as_deref()
        .and_then(|name| crate::slug::derive_slug(name).ok())
        .map_or_else(|| source_page.to_owned(), |slug| format!("{slug}.md"))
}

/// The three rules of the module docs, applied to the claims still waiting
/// in the capture buffer. A buffered claim has no page and names no wiki, so
/// there is nothing to move and nothing to strip.
async fn forget_captures(
    pool: &SqlitePool,
    user_id: &str,
    gone: &Principal,
    tombstone: &Principal,
    report: &mut ForgetReport,
) -> Result<()> {
    let wire = gone.to_string();
    let dead = tombstone.to_string();
    report.captures_dropped = sqlx::query(
        "DELETE FROM capture_buffer
          WHERE subject_id = ?1
            AND (sender_id IS NULL OR sender_id = ?1 OR sender_id = ?2)",
    )
    .bind(&wire)
    .bind(&dead)
    .execute(pool)
    .await?
    .rows_affected();
    report.captures_handed_over = sqlx::query(
        "UPDATE capture_buffer
            SET subject_id = sender_id,
                subject_external = COALESCE(subject_external, ?3)
          WHERE subject_id = ?1
            AND sender_id IS NOT NULL AND sender_id <> ?1 AND sender_id <> ?2",
    )
    .bind(&wire)
    .bind(&dead)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    sqlx::query("UPDATE capture_buffer SET sender_id = ?2 WHERE sender_id = ?1")
        .bind(&wire)
        .bind(&dead)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete every row across [`PERSONAL_ROWS`] that names them, plus the
/// segments of the document jobs that go with it.
async fn drop_personal_rows(pool: &SqlitePool, user_id: &str) -> Result<u64> {
    // A job's segments hold the document text; they carry no user id of
    // their own, so they go by their job.
    sqlx::query(
        "DELETE FROM document_job_segments
          WHERE job_id IN (SELECT job_id FROM document_jobs WHERE subject_id = ?)",
    )
    .bind(Principal::User(user_id.to_owned()).to_string())
    .execute(pool)
    .await?;

    let mut removed = 0;
    for (table, column, form) in PERSONAL_ROWS {
        let sql = format!("DELETE FROM {table} WHERE {column} = ?");
        removed += sqlx::query(&sql)
            .bind(spell(user_id, *form))
            .execute(pool)
            .await?
            .rows_affected();
    }
    Ok(removed)
}

/// Delete the notices and the proposals addressed to them. A question put to
/// somebody who is gone can never be answered, and the answer it was waiting
/// for is not one anybody else may give.
async fn drop_notices(pool: &SqlitePool, gone: &Principal) -> Result<u64> {
    let wire = gone.to_string();
    let mut removed =
        sqlx::query("DELETE FROM wiki_events WHERE json_extract(payload, '$.recipient_id') = ?")
            .bind(&wire)
            .execute(pool)
            .await?
            .rows_affected();
    removed += sqlx::query("DELETE FROM structure_proposals WHERE recipient_id = ?")
        .bind(&wire)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed)
}

/// Re-stamp every audit column that names them with [`removed_sender`].
async fn anonymise_audit(pool: &SqlitePool, user_id: &str) -> Result<u64> {
    let mut touched = 0;
    for (table, column, form) in AUDIT_COLUMNS {
        let sql = format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1");
        touched += sqlx::query(&sql)
            .bind(spell(user_id, *form))
            .bind(spell(REMOVED_USER_ID, *form))
            .execute(pool)
            .await?
            .rows_affected();
    }
    Ok(touched)
}

/// Strike their id out of every JSON array in [`MEMBERSHIP_LISTS`].
async fn strike_from_lists(pool: &SqlitePool, user_id: &str, gone: &Principal) -> Result<u64> {
    let mut amended = 0;
    for (table, column, form) in MEMBERSHIP_LISTS {
        let needle = match form {
            IdForm::Bare => user_id.to_owned(),
            IdForm::Wire => gone.to_string(),
        };
        let sql = format!(
            "UPDATE {table}
                SET {column} = (SELECT json_group_array(kept.value)
                                  FROM json_each({table}.{column}) kept
                                 WHERE kept.value <> ?1)
              WHERE {column} IS NOT NULL
                AND json_valid({column})
                AND EXISTS (SELECT 1 FROM json_each({table}.{column}) hit
                             WHERE hit.value = ?1)"
        );
        amended += sqlx::query(&sql)
            .bind(&needle)
            .execute(pool)
            .await?
            .rows_affected();
    }
    Ok(amended)
}

/// Smart wikis outside their own subtree that still declare them as owner.
///
/// Reported rather than erased: a wiki that sits outside somebody's own
/// subtree may be one other people are reading, and the dashboard's
/// orphaned-wiki delete is where that call belongs. While the id stays
/// unclaimed no enrolled principal matches their scope, so nobody reads them
/// — which is also why the report names them: enrolling somebody under the
/// same id later would hand them the leftovers.
async fn orphan_smart_wikis(
    pool: &SqlitePool,
    gone: &Principal,
    hers: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT wiki_id FROM smart_wikis WHERE owner_id = ? ORDER BY wiki_id")
            .bind(gone.to_string())
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.0)
        .filter(|id| !hers.contains(id))
        .collect())
}

/// Erase every subtree of theirs an earlier wiki delete parked in
/// `<workdir>/trash/`.
///
/// A trashed directory is named `<wiki_id>__<stamp>`, and a sub-wiki's id is
/// its parent's plus a dash, so both `alice__…` and `alice-garden__…` are
/// theirs. Best-effort: a directory that will not go is logged, and the rest
/// of the erasure stands.
fn erase_trashed_subtrees(tree: &WikiTree, user_id: &str) -> usize {
    let trash_root = tree.workdir().join("trash");
    let Ok(entries) = std::fs::read_dir(&trash_root) else {
        return 0;
    };
    let mut erased = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(wiki_id) = name.split("__").next() else {
            continue;
        };
        let theirs = wiki_id == user_id || wiki_id.starts_with(&format!("{user_id}-"));
        if !theirs {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => erased += 1,
            Err(e) => tracing::warn!(
                dir = %entry.path().display(),
                error = %e,
                "gdpr: a trashed subtree of this person could not be erased"
            ),
        }
    }
    erased
}

/// Spell a bare user id the way one column stores it.
fn spell(user_id: &str, form: IdForm) -> String {
    match form {
        IdForm::Bare => user_id.to_owned(),
        IdForm::Wire => Principal::User(user_id.to_owned()).to_string(),
    }
}

// ---------- Export ----------

/// Counters of one [`export_user`] run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportUserReport {
    /// Entries taken from their own wiki subtree (pages, `_meta.md` files
    /// and the media that subtree references).
    pub wiki_entries: usize,
    /// Facts about them held in wikis that are not theirs, written to
    /// `elsewhere.md`.
    pub facts_elsewhere: usize,
    /// Media items they uploaded, bundled under `media/`.
    pub media_bundled: usize,
    /// Media items whose blob is missing from the store, so they could not
    /// travel. Counted, never silently dropped.
    pub media_missing: usize,
}

/// One finished person archive.
#[derive(Debug)]
pub struct UserExport {
    /// Complete uncompressed tar stream, served as `application/x-tar`.
    pub tar_bytes: Vec<u8>,
    /// The archive's top-level directory (the person's id).
    pub root_dir: String,
    /// Counters of the run.
    pub report: ExportUserReport,
}

/// Everything this memory holds about one person, as a portable archive.
///
/// Four things, under a directory named after them:
///
/// - `enrollment.json` — their card: id, aliases, email, language, timezone,
///   the groups they belong to and the consumer grants that name them.
/// - `wiki/` — their own wiki subtree, exactly as
///   [`export::export_wiki_subtree`] renders it: every DB-known region
///   rewritten to the self-describing full-marker form, so the pages stand
///   alone without `engine.db` beside them.
/// - `elsewhere.md` — every fact about them that is **not** in their own
///   wiki, each with where it is filed, who said it, and when. Without it an
///   export would be their autobiography and not what the memory knows.
/// - `media/` — the files they uploaded, plus `_catalog.json` describing
///   each.
///
/// Returns `Ok(None)` when no such user is enrolled.
///
/// # Errors
///
/// As the engine layer the step failed in.
pub async fn export_user(
    pool: &SqlitePool,
    tree: &WikiTree,
    user_id: &str,
) -> Result<Option<UserExport>> {
    let card = enrollment_card(pool, user_id).await?;
    let Some(card) = card else {
        return Ok(None);
    };
    let gone = Principal::User(user_id.to_owned());
    let root = PathBuf::from(user_id);
    let mut builder = tar::Builder::new(Vec::new());
    let mut report = ExportUserReport::default();

    export::append_entry(
        &mut builder,
        &root.join("enrollment.json"),
        &serde_json::to_vec_pretty(&card)?,
    )?;

    // Their own wiki subtree. The wiki export already answers "what does a
    // page look like on its own", media included, so it is re-rooted under
    // `wiki/` rather than re-derived.
    let hers: BTreeSet<String> = if let Ok(wiki_id) = WikiId::parse(user_id)
        && tree.locate(&wiki_id).is_ok()
    {
        let archive = export::export_wiki_subtree(pool, tree, &wiki_id).await?;
        report.wiki_entries = graft(&archive.tar_bytes, &root.join("wiki"), &mut builder)?;
        wiki_delete::collect_subtree(tree, &wiki_id)?
            .into_iter()
            .map(|d| d.meta.wiki_id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };

    // What other people's wikis hold about them.
    let elsewhere: Vec<FactIndexRow> = fact_index::find_active_by_subject(pool, &gone)
        .await?
        .into_iter()
        .filter(|row| !hers.contains(&row.wiki_id))
        .collect();
    report.facts_elsewhere = elsewhere.len();
    export::append_entry(
        &mut builder,
        &root.join("elsewhere.md"),
        render_elsewhere(user_id, &elsewhere).as_bytes(),
    )?;

    // What they uploaded.
    let media_dir = root.join("media");
    let mut manifest = Vec::new();
    for row in media::find_by_subject(pool, &gone).await? {
        let blob = media::blob_path(tree.workdir(), &row.sha256);
        match std::fs::read(&blob) {
            Ok(bytes) => {
                export::append_entry(
                    &mut builder,
                    &media_dir.join(row.catalog_id.as_str()),
                    &bytes,
                )?;
                manifest.push(export::MediaManifestEntry::from_row(row));
                report.media_bundled += 1;
            },
            Err(e) => {
                tracing::warn!(
                    catalog_id = row.catalog_id.as_str(),
                    error = %e,
                    "gdpr export: blob missing from the store"
                );
                report.media_missing += 1;
            },
        }
    }
    if !manifest.is_empty() {
        export::append_entry(
            &mut builder,
            &media_dir.join("_catalog.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
    }

    Ok(Some(UserExport {
        tar_bytes: builder.into_inner()?,
        root_dir: user_id.to_owned(),
        report,
    }))
}

/// The person's card as the archive carries it.
#[derive(Debug, serde::Serialize)]
struct EnrollmentCard {
    user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    is_admin: bool,
    /// This identity is a consumer agent's, not a person's.
    is_agent: bool,
    /// Groups they belong to, with the operator's scope prose.
    groups: Vec<CardGroup>,
    /// Consumers allowed to act as them, and the consumer whose own system
    /// identity they are.
    delegated_consumers: Vec<String>,
}

/// One group on the person's card.
#[derive(Debug, serde::Serialize)]
struct CardGroup {
    group_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

/// Read the person's card, or `None` when they are not enrolled.
async fn enrollment_card(pool: &SqlitePool, user_id: &str) -> Result<Option<EnrollmentCard>> {
    type Row = (
        Option<String>,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT email, aliases, is_admin, is_agent, locale, timezone
           FROM enrollment_users WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some((email, aliases_json, is_admin, is_agent, locale, timezone)) = row else {
        return Ok(None);
    };
    let groups: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT group_id, scope FROM enrollment_groups g, json_each(g.members) m
          WHERE m.value = ? ORDER BY group_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let delegated: Vec<(String,)> = sqlx::query_as(
        "SELECT consumer_id FROM consumers WHERE system_user_id = ?
          UNION
         SELECT d.consumer_id FROM consumer_delegations d, json_each(d.allowed_sender_ids) j
          WHERE j.value = ?
         ORDER BY 1",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(EnrollmentCard {
        user_id: user_id.to_owned(),
        email,
        aliases: aliases_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default(),
        locale,
        timezone,
        is_admin: is_admin != 0,
        is_agent: is_agent != 0,
        groups: groups
            .into_iter()
            .map(|(group_id, scope)| CardGroup { group_id, scope })
            .collect(),
        delegated_consumers: delegated.into_iter().map(|c| c.0).collect(),
    }))
}

/// Copy every entry of `archive` into `builder`, re-rooted under `prefix`.
///
/// The wiki archive is rooted at the wiki's own directory name; the person
/// archive puts that whole tree under one directory of its own, so the first
/// path component is dropped and `prefix` put in its place. Returns the
/// number of entries grafted.
fn graft(archive: &[u8], prefix: &Path, builder: &mut tar::Builder<Vec<u8>>) -> Result<usize> {
    use std::io::Read as _;

    let mut source = tar::Archive::new(archive);
    let mut grafted = 0;
    for entry in source.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut components = path.components();
        components.next();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        export::append_entry(builder, &prefix.join(components.as_path()), &bytes)?;
        grafted += 1;
    }
    Ok(grafted)
}

/// Render the facts other people's wikis hold about this person, each with
/// its provenance.
fn render_elsewhere(user_id: &str, rows: &[FactIndexRow]) -> String {
    use std::fmt::Write as _;

    let mut out = format!("# What other wikis hold about {user_id}\n\n");
    if rows.is_empty() {
        out.push_str(
            "Nothing: every fact this memory holds about this person is in their own wiki.\n",
        );
        return out;
    }
    let _ = writeln!(
        out,
        "{n} {noun}, each filed in a wiki that is not theirs. For every one: where it \
         is filed, who said it, and when.\n",
        n = rows.len(),
        noun = if rows.len() == 1 { "fact" } else { "facts" },
    );
    for row in rows {
        let _ = writeln!(out, "## {}", row.source_path);
        let _ = writeln!(out, "- **wiki:** `{}`", row.wiki_id);
        let _ = writeln!(
            out,
            "- **said by:** {}",
            row.sender_id
                .as_ref()
                .map_or_else(|| "unrecorded".to_owned(), ToString::to_string)
        );
        let _ = writeln!(out, "- **recorded:** {}", row.created_at);
        if let Some(kind) = row.fact_type.as_deref() {
            let _ = writeln!(out, "- **type:** {kind}");
        }
        if !row.topics.is_empty() {
            let _ = writeln!(out, "- **topics:** {}", row.topics.join(", "));
        }
        if row.valid_from.is_some() || row.valid_to.is_some() {
            let _ = writeln!(
                out,
                "- **holds:** from {} to {}",
                row.valid_from.as_deref().unwrap_or("always"),
                row.valid_to.as_deref().unwrap_or("open")
            );
        }
        let _ = writeln!(out, "- **readable by:** {}", audience_line(row));
        out.push('\n');
        for line in row.text.lines() {
            let _ = writeln!(out, "> {line}");
        }
        out.push('\n');
    }
    out
}

/// The principals a fact grants read to, in the order the ACL evaluates
/// them: subject, then the read extension, then the sender.
fn audience_line(row: &FactIndexRow) -> String {
    let mut who = vec![row.subject_id.to_string()];
    who.extend(row.allow_ids.iter().map(ToString::to_string));
    if let Some(sender) = row.sender_id.as_ref() {
        who.push(sender.to_string());
    }
    who.join(", ")
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::path::PathBuf;

    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::{ForgetReport, REMOVED_USER_ID, export_user, forget_user, removed_sender};
    use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
    use crate::embedder::{Embedder, FakeEmbedder};
    use crate::enrollment;
    use crate::fact_index;
    use crate::media;
    use crate::types::{FactId, Principal, WikiId};
    use crate::wiki::{META_FILENAME, WikiTree};

    fn embedder() -> std::sync::Arc<dyn Embedder> {
        std::sync::Arc::new(FakeEmbedder::new("fake", 4))
    }

    /// A workdir with `engine.db` and `wikis/` beside each other — the real
    /// layout, which the media blob store and the tree both read off.
    async fn workdir() -> (TempDir, SqlitePool, WikiTree) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        (dir, pool, tree)
    }

    /// Seed one identity wiki on disk.
    fn seed_wiki(tree: &WikiTree, id: &str) {
        let dir = tree.wikis_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            "---\nwiki_id: {id}\nwiki_type: wiki-user\nparent_wiki_id: null\n\
             slug: {id}\ntitle: {id}\nacl_default: 'user:{id}'\n---\n"
        );
        std::fs::write(dir.join(META_FILENAME), meta).unwrap();
    }

    async fn enrol(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES (?, '[]', 0)",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Write one fact onto a page, with the three axes spelled out.
    async fn capture(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        subject: &str,
        sender: &str,
        body: &str,
    ) -> FactId {
        let req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: subject.parse::<Principal>().unwrap(),
            allow: vec![],
            sender: Some(sender.parse::<Principal>().unwrap()),
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

    /// The four sentences the founder's rule sorts differently, plus a
    /// photo and an audit row, then the erasure. Returns the ids so each
    /// test can ask about its own.
    struct Scene {
        _dir: TempDir,
        pool: SqlitePool,
        tree: WikiTree,
        bob_about_alice: FactId,
        alice_about_self: FactId,
        alice_about_bob: FactId,
        rule_about_alice: FactId,
        alice_in_bobs_wiki: FactId,
        bobs_secret: FactId,
        photo: media::MediaRow,
        report: ForgetReport,
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one scene: the four sentences the rule sorts differently, plus a photo, a grant list and an audit row"
    )]
    async fn forget_alice() -> Scene {
        let (dir, pool, tree) = workdir().await;
        seed_wiki(&tree, "alice");
        seed_wiki(&tree, "bob");
        let tree = WikiTree::open(dir.path()).unwrap();
        enrol(&pool, "alice").await;
        enrol(&pool, "bob").await;

        let bob_about_alice = capture(
            &tree,
            &pool,
            "alice",
            "cucina.md",
            "user:alice",
            "user:bob",
            "alice did a great job on the client presentation",
        )
        .await;
        let alice_about_self = capture(
            &tree,
            &pool,
            "alice",
            "cucina.md",
            "user:alice",
            "user:alice",
            "alice cannot stand olives",
        )
        .await;
        let alice_about_bob = capture(
            &tree,
            &pool,
            "bob",
            "cucina.md",
            "user:bob",
            "user:alice",
            "bob prefers tea to coffee",
        )
        .await;
        let rule_about_alice = capture(
            &tree,
            &pool,
            "alice",
            crate::wiki::RULES_FILENAME,
            "user:alice",
            "user:bob",
            "always write to alice in English",
        )
        .await;

        // Her own claim about herself, but filed on a page of bob's wiki —
        // the page survives, so her sentence has to be cut out of it.
        let alice_in_bobs_wiki = capture(
            &tree,
            &pool,
            "bob",
            "diario.md",
            "user:alice",
            "user:alice",
            "alice is allergic to hazelnuts",
        )
        .await;

        // A fact of bob's that is neither about her nor by her: she was only
        // allowed to read it.
        let bobs_secret = capture(
            &tree,
            &pool,
            "bob",
            "diario.md",
            "user:bob",
            "user:bob",
            "bob is saving up for a bicycle",
        )
        .await;
        fact_index::set_acl(
            &pool,
            &bobs_secret,
            &"user:bob".parse().unwrap(),
            &["user:alice".parse().unwrap()],
            Some(&"user:bob".parse().unwrap()),
        )
        .await
        .unwrap();

        // A group she belongs to, and a consumer allowed to speak as her.
        sqlx::query(
            "INSERT INTO enrollment_groups (group_id, members, scope)
             VALUES ('famiglia', '[\"alice\",\"bob\"]', 'the household')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO consumer_delegations
                 (consumer_id, allowed_sender_ids, granted_at, granted_by)
             VALUES ('assistant', '[\"alice\",\"bob\"]', '2026-09-06T00:00:00Z', 'carol')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // A subtree of hers an earlier wiki delete parked in the trash.
        std::fs::create_dir_all(dir.path().join("trash/alice__20260101T000000Z")).unwrap();
        std::fs::write(
            dir.path().join("trash/alice__20260101T000000Z/cucina.md"),
            "an old copy of her memory",
        )
        .unwrap();

        let photo = media::store_media(
            &pool,
            dir.path(),
            media::NewMedia {
                bytes: b"not really a png".to_vec(),
                kind: media::kind::PHOTO.to_owned(),
                mime: "image/png".to_owned(),
                subject: "user:alice".parse().unwrap(),
                uploaded_by_consumer: None,
                caption: None,
                description: None,
                original_filename: Some("holiday.png".to_owned()),
            },
        )
        .await
        .unwrap()
        .row;

        crate::audit::record(
            &pool,
            &crate::audit::ToolExecutionInput {
                tool_name: "wiki_recall",
                sender_id: "alice",
                device_label: "laptop",
                rate_limit_id: None,
                args_hash: None,
                result_summary: None,
                latency_ms: 0,
                cost_estimate: None,
                error: None,
            },
        )
        .await
        .unwrap();

        let report = forget_user(&pool, &tree, embedder(), "alice")
            .await
            .unwrap()
            .expect("alice was enrolled");
        Scene {
            _dir: dir,
            pool,
            tree,
            bob_about_alice,
            alice_about_self,
            alice_about_bob,
            rule_about_alice,
            alice_in_bobs_wiki,
            bobs_secret,
            photo,
            report,
        }
    }

    async fn row(pool: &SqlitePool, id: &FactId) -> fact_index::FactIndexRow {
        fact_index::find_by_id(pool, id)
            .await
            .unwrap()
            .expect("the row is never dropped, only retired")
    }

    /// The founder's own example. Bob's memory of his colleague's work
    /// survives her erasure: it changes hands, it keeps her name as a plain
    /// external subject, and it moves onto a page of HIS wiki named after
    /// her. The denial is the one that matters — it is not tombstoned, and
    /// it is not left in the wiki that no longer exists.
    #[tokio::test]
    async fn what_somebody_else_said_about_her_becomes_theirs() {
        let scene = forget_alice().await;
        let fact = row(&scene.pool, &scene.bob_about_alice).await;

        assert!(
            fact.deleted_at.is_none(),
            "somebody else's memory of her is not hers to destroy"
        );
        assert_eq!(fact.subject_id, "user:bob".parse::<Principal>().unwrap());
        assert_eq!(
            fact.subject_external.as_deref(),
            Some("alice"),
            "her name survives as a name, which grants nothing"
        );
        assert_eq!(fact.wiki_id, "bob");
        assert_eq!(
            fact.source_path, "wikis/bob/alice.md",
            "it lands on the page named after what it is about"
        );
        assert_eq!(scene.report.facts_handed_over, 1);
        assert_eq!(scene.report.facts_moved, 1);
    }

    /// What she said about herself is hers, and it goes — row and all, not
    /// as a tombstone that keeps the sentence, and NOT handed to anybody,
    /// which is the other thing the rule could have said.
    #[tokio::test]
    async fn what_she_said_about_herself_is_destroyed() {
        let scene = forget_alice().await;

        assert!(
            fact_index::find_by_id(&scene.pool, &scene.alice_about_self)
                .await
                .unwrap()
                .is_none(),
            "an erasure keeps no tombstone: the row holds the sentence"
        );
        assert_eq!(scene.report.facts_left_as_tombstone, 0);
        let handed_to_bob: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM fact_index WHERE subject_external = 'alice'")
                .fetch_one(&scene.pool)
                .await
                .unwrap();
        assert_eq!(
            handed_to_bob, 1,
            "only the fact bob told changed hands; hers was destroyed, not passed on"
        );
    }

    /// Her own sentence filed on somebody else's page: the row goes and the
    /// prose is cut out of the page that survives, so nothing of hers is left
    /// readable there.
    #[tokio::test]
    async fn her_sentence_is_cut_out_of_the_page_that_survives() {
        let scene = forget_alice().await;

        assert!(
            fact_index::find_by_id(&scene.pool, &scene.alice_in_bobs_wiki)
                .await
                .unwrap()
                .is_none()
        );
        let page = scene.tree.workdir().join("wikis/bob/diario.md");
        let raw = std::fs::read_to_string(&page).unwrap();
        assert!(
            !raw.contains("allergic to hazelnuts"),
            "her sentence is off the page, not merely hidden: {raw}"
        );
        assert!(
            raw.contains("saving up for a bicycle"),
            "and bob's own fact on the same page is untouched: {raw}"
        );
        assert_eq!(
            scene.report.facts_left_as_tombstone, 0,
            "nothing was left half-erased"
        );
    }

    /// What she said about somebody else is that person's memory: it stays
    /// exactly where it was filed, and only the name of who said it goes.
    #[tokio::test]
    async fn what_she_said_about_others_stays_and_loses_her_name() {
        let scene = forget_alice().await;
        let fact = row(&scene.pool, &scene.alice_about_bob).await;

        assert!(fact.deleted_at.is_none(), "bob's memory is not destroyed");
        assert_eq!(fact.wiki_id, "bob", "it does not move: it was already home");
        assert_eq!(fact.source_path, "wikis/bob/cucina.md");
        assert_eq!(fact.sender_id, Some(removed_sender()));
        assert_eq!(scene.report.facts_disowned, 1);
    }

    /// The one exception, and the reason for it: a rule is an instruction,
    /// not a memory. Handed to whoever wrote it, "always write to alice in
    /// English" would become an instruction about **bob**. So it is
    /// destroyed even though somebody else wrote it.
    #[tokio::test]
    async fn a_rule_about_her_is_destroyed_even_when_somebody_else_wrote_it() {
        let scene = forget_alice().await;

        assert!(
            fact_index::find_by_id(&scene.pool, &scene.rule_about_alice)
                .await
                .unwrap()
                .is_none(),
            "a directive about her dies with her"
        );
        let in_bobs_wiki: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fact_index WHERE wiki_id = 'bob' AND \"text\" LIKE '%in English%'",
        )
        .fetch_one(&scene.pool)
        .await
        .unwrap();
        assert_eq!(
            in_bobs_wiki, 0,
            "it was NOT handed over to its author the way an ordinary fact is"
        );
    }

    /// The audit trail keeps what happened and loses who did it. Both halves
    /// are asserted: the row is still there, and her name is not.
    #[tokio::test]
    async fn the_audit_trail_survives_without_her_name() {
        let scene = forget_alice().await;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_executions")
            .fetch_one(&scene.pool)
            .await
            .unwrap();
        assert_eq!(total, 1, "the record that a call happened is not deleted");
        let hers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tool_executions WHERE sender_id = 'alice'")
                .fetch_one(&scene.pool)
                .await
                .unwrap();
        assert_eq!(hers, 0, "her name is gone from it");
        let anonymous: String = sqlx::query_scalar("SELECT sender_id FROM tool_executions LIMIT 1")
            .fetch_one(&scene.pool)
            .await
            .unwrap();
        assert_eq!(anonymous, REMOVED_USER_ID);
        assert!(scene.report.audit_rows_anonymised >= 1);
    }

    /// Forgetting is not the recoverable wiki delete: the directory is
    /// erased where it stood, and nothing of hers waits in the trash.
    #[tokio::test]
    async fn her_wiki_is_erased_and_nothing_waits_in_the_trash() {
        let scene = forget_alice().await;
        let workdir = scene.tree.workdir();

        assert!(
            !workdir.join("wikis").join("alice").exists(),
            "her wiki is gone from the tree"
        );
        let trashed: Vec<String> = std::fs::read_dir(workdir.join("trash"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            trashed.is_empty(),
            "an erasure keeps no copy, and a trashed subtree is a copy: {trashed:?}"
        );
        assert_eq!(scene.report.wikis_erased, 1);

        let still_enrolled: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM enrollment_users WHERE user_id = 'alice'")
                .fetch_optional(&scene.pool)
                .await
                .unwrap();
        assert!(still_enrolled.is_none());
    }

    /// The files she uploaded go, catalog row and bytes together.
    #[tokio::test]
    async fn her_uploads_go_with_her() {
        let scene = forget_alice().await;

        assert!(
            media::find_by_id(&scene.pool, &scene.photo.catalog_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !media::blob_path(scene.tree.workdir(), &scene.photo.sha256).exists(),
            "the bytes go too when nobody else uploaded them"
        );
        assert_eq!(scene.report.media_removed, 1);
        assert_eq!(scene.report.media_blobs_removed, 1);
    }

    /// Running it twice finds nothing the second time — an erasure is a
    /// state, not an operation you can repeat into a different result.
    #[tokio::test]
    async fn a_second_erasure_finds_nobody() {
        let scene = forget_alice().await;
        let again = forget_user(&scene.pool, &scene.tree, embedder(), "alice")
            .await
            .unwrap();
        assert!(again.is_none());
    }

    /// The training spool holds whole prompts, so it holds her, and it is
    /// emptied whole.
    ///
    /// A spool record has no subject and no sender column — the person is
    /// inside `request.prompt`, as free text — so there is no honest way to
    /// take out her lines and leave the rest, and a filter keyed on her id
    /// would leave behind every prompt that describes her without naming her.
    /// The files stay, empty, so today's file keeps taking appends.
    #[tokio::test]
    async fn the_training_spool_is_emptied_because_a_prompt_has_no_subject() {
        let (dir, pool, tree) = workdir().await;
        seed_wiki(&tree, "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        enrol(&pool, "alice").await;

        let spool = dir.path().join(crate::training_spool::TRAINING_SPOOL_DIR);
        std::fs::create_dir_all(&spool).unwrap();
        // One line naming her, one that describes her without naming her.
        std::fs::write(
            spool.join("2026-09-06.jsonl"),
            "{\"function\":\"ingest\",\"request\":{\"prompt\":\"sender_id: alice\"}}\n\
             {\"function\":\"cronista\",\"request\":{\"prompt\":\"her check-up is on Thursday\"}}\n",
        )
        .unwrap();
        std::fs::write(
            spool.join("2026-09-07.jsonl"),
            "{\"function\":\"navigator\"}\n",
        )
        .unwrap();
        // Not a spool file, and not this pass's to touch.
        std::fs::write(spool.join("README.md"), "notes\n").unwrap();

        let report = forget_user(&pool, &tree, embedder(), "alice")
            .await
            .unwrap()
            .expect("she was enrolled");

        assert_eq!(report.training_spool_files_emptied, 2);
        for day in ["2026-09-06.jsonl", "2026-09-07.jsonl"] {
            let path = spool.join(day);
            assert!(path.is_file(), "{day} must still be there to append to");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "",
                "{day} still holds prompts"
            );
        }
        assert_eq!(
            std::fs::read_to_string(spool.join("README.md")).unwrap(),
            "notes\n",
            "only the spool files are emptied"
        );
    }

    /// The id is spent, and only that id.
    ///
    /// What survives the erasure still says her name — bob's fact keeps it in
    /// `subject_external` — so an admin re-using the id would hand all of it
    /// to a different person, and the erasure would have moved the material
    /// rather than removed it. A neighbouring id is nobody's and passes.
    #[tokio::test]
    async fn her_id_is_refused_afterwards_and_a_free_one_is_not() {
        let scene = forget_alice().await;

        assert!(
            enrollment::is_forgotten(&scene.pool, "alice")
                .await
                .unwrap(),
            "the erasure records the id it spent"
        );
        let refusal = enrollment::reject_if_forgotten(&scene.pool, "alice")
            .await
            .expect_err("her id must not be handed to a new account");
        assert!(
            refusal.contains("alice") && refusal.contains("erased"),
            "the refusal says whose id and why: {refusal}"
        );
        // It is read by a person, so it is a sentence: a run of spaces means
        // a wrapped literal lost its continuation on the way in.
        assert!(
            !refusal.contains("  "),
            "the refusal reads as prose, not as a wrapped literal: {refusal:?}"
        );
        enrollment::reject_if_forgotten(&scene.pool, "alice2")
            .await
            .expect("a neighbouring id was never anybody's");

        // And the roster sync refuses it too, which is the path a YAML
        // import and the group editor both go through.
        let file = enrollment::EnrollmentFile {
            version: 1,
            users: vec![
                enrollment::UserEntry {
                    id: "alice".to_owned(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                },
                enrollment::UserEntry {
                    id: "bob".to_owned(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                },
            ],
            groups: Vec::new(),
        };
        let err = enrollment::mirror_to_db(&scene.pool, &file)
            .await
            .expect_err("the roster may not re-enrol her id");
        assert!(
            matches!(err, enrollment::EnrollmentError::IdWasForgotten(_)),
            "{err:?}"
        );
        // Refused before anything was deleted: bob is still enrolled.
        assert!(
            enrollment::list_users(&scene.pool)
                .await
                .unwrap()
                .iter()
                .any(|u| u.user_id == "bob"),
            "a refused sync leaves the roster it was going to replace"
        );
    }

    /// Being allowed to read somebody else's fact is not a memory of her:
    /// her name comes off the read list and the fact itself is untouched.
    #[tokio::test]
    async fn her_name_comes_off_other_peoples_read_lists() {
        let scene = forget_alice().await;
        let fact = row(&scene.pool, &scene.bobs_secret).await;

        assert!(fact.deleted_at.is_none(), "it is bob's fact, and it stays");
        assert_eq!(fact.subject_id, "user:bob".parse::<Principal>().unwrap());
        assert_eq!(fact.sender_id, Some("user:bob".parse().unwrap()));
        assert!(
            fact.allow_ids.is_empty(),
            "the only thing that changes is that she is no longer on the list: {:?}",
            fact.allow_ids
        );
        assert_eq!(scene.report.allow_lists_pruned, 1);
    }

    /// The lists that grant something by naming her — her group, and the
    /// consumer allowed to speak as her — lose her and keep everybody else.
    /// And a subtree of hers a previous wiki delete parked in the trash is
    /// erased too: it is a full copy of her memory in cleartext.
    #[tokio::test]
    async fn her_grants_are_struck_out_and_the_old_trash_goes() {
        let scene = forget_alice().await;

        let members: String =
            sqlx::query_scalar("SELECT members FROM enrollment_groups WHERE group_id = 'famiglia'")
                .fetch_one(&scene.pool)
                .await
                .unwrap();
        assert_eq!(members, r#"["bob"]"#);
        let delegated: String = sqlx::query_scalar(
            "SELECT allowed_sender_ids FROM consumer_delegations WHERE consumer_id = 'assistant'",
        )
        .fetch_one(&scene.pool)
        .await
        .unwrap();
        assert_eq!(delegated, r#"["bob"]"#);

        assert!(
            !scene
                .tree
                .workdir()
                .join("trash/alice__20260101T000000Z")
                .exists(),
            "an erasure reaches the copies an earlier delete left behind"
        );
        assert_eq!(scene.report.trash_dirs_erased, 1);
    }

    /// Forget the speaker first, then the subject. What bob said about alice
    /// became authorless when bob went; when alice goes too there is nobody
    /// left whose memory it is, so it is destroyed rather than handed to an
    /// identity nobody holds.
    #[tokio::test]
    async fn a_fact_whose_keeper_was_already_forgotten_is_destroyed() {
        let (dir, pool, tree) = workdir().await;
        seed_wiki(&tree, "alice");
        seed_wiki(&tree, "bob");
        let tree = WikiTree::open(dir.path()).unwrap();
        enrol(&pool, "alice").await;
        enrol(&pool, "bob").await;
        let fact = capture(
            &tree,
            &pool,
            "alice",
            "cucina.md",
            "user:alice",
            "user:bob",
            "alice did a great job on the client presentation",
        )
        .await;

        forget_user(&pool, &tree, embedder(), "bob").await.unwrap();
        assert_eq!(
            row(&pool, &fact).await.sender_id,
            Some(removed_sender()),
            "bob's erasure leaves the fact standing, authorless"
        );

        forget_user(&pool, &tree, embedder(), "alice")
            .await
            .unwrap()
            .expect("alice is enrolled");
        assert!(
            fact_index::find_by_id(&pool, &fact)
                .await
                .unwrap()
                .is_none(),
            "nobody is left to keep it, so it goes"
        );
        let handed_to_nobody: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fact_index WHERE subject_id = 'user:_removed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            handed_to_nobody, 0,
            "it is never handed to an identity nobody holds"
        );
    }

    /// The export is what the memory knows about her, and the half that
    /// makes it more than an autobiography is `elsewhere.md`: the facts
    /// held in **other people's** wikis, each with who said it.
    #[tokio::test]
    async fn the_export_carries_what_other_wikis_hold_about_her() {
        let (dir, pool, tree) = workdir().await;
        seed_wiki(&tree, "alice");
        seed_wiki(&tree, "bob");
        let tree = WikiTree::open(dir.path()).unwrap();
        enrol(&pool, "alice").await;
        enrol(&pool, "bob").await;
        capture(
            &tree,
            &pool,
            "alice",
            "cucina.md",
            "user:alice",
            "user:alice",
            "alice cannot stand olives",
        )
        .await;
        capture(
            &tree,
            &pool,
            "bob",
            "diario.md",
            "user:alice",
            "user:bob",
            "alice did a great job on the client presentation",
        )
        .await;

        let export = export_user(&pool, &tree, "alice")
            .await
            .unwrap()
            .expect("alice is enrolled");
        assert_eq!(export.report.facts_elsewhere, 1);

        let mut entries = std::collections::BTreeMap::new();
        let mut archive = tar::Archive::new(&export.tar_bytes[..]);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut body = String::new();
            let _ = entry.read_to_string(&mut body);
            entries.insert(path, body);
        }

        let elsewhere = entries
            .get("alice/elsewhere.md")
            .expect("the archive names what other wikis hold");
        assert!(
            elsewhere.contains("alice did a great job"),
            "the fact bob's wiki holds about her travels: {elsewhere}"
        );
        assert!(
            elsewhere.contains("user:bob"),
            "with who said it: {elsewhere}"
        );
        assert!(
            !elsewhere.contains("cannot stand olives"),
            "her own wiki is the `wiki/` half, not this one: {elsewhere}"
        );
        assert!(entries.contains_key("alice/wiki/_meta.md"), "{entries:?}");
        assert!(
            entries
                .get("alice/enrollment.json")
                .is_some_and(|card| card.contains("\"user_id\": \"alice\"")),
            "{entries:?}"
        );
    }
}
