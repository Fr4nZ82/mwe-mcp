// SPDX-License-Identifier: AGPL-3.0-or-later
//! Non-sender owner's **forget request** and the audience vote that resolves it
//! (the fact-forget vote).
//!
//! "ACL lives only in the fact." A fact's **sender** (its author) deletes their
//! own contribution directly ([`crate::acl::can_delete`]); an admin acts on any
//! fact. But a non-sender **`owner`** — the fact's *subject*, or a member of an
//! owning group — who did not author it has no such authority. Their path is a
//! **request the fact's audience votes on**, built here.
//!
//! ## Propose-first, silence = consent
//!
//! This is the **opposite** lifecycle of the act-first governed page deletion
//! ([`crate::page`]):
//!
//! - [`open_forget_request`] opens a **pending** `fact_forget` proposal
//!   ([`crate::proposals::kind::FACT_FORGET`]); **the fact stays active** during
//!   the window — the requester has no authority to remove a contribution they
//!   did not author, so nothing is destroyed up front.
//! - the **eligible voters** are the fact's [`crate::acl::can_read`] audience
//!   (`owner ∪ allow ∪ {sender}`, groups expanded, `global` dropped — a public
//!   fact has no finite electorate), **minus the requester** (who consented by
//!   asking). The sender (author) is always in the audience.
//! - a **NO-majority** within the window (`no * 2 > eligible`) **blocks** it: the
//!   proposal is expired and the fact stays ([`VoteOutcome::Rejected`]).
//! - **silence is consent**: at the window's close with no NO-majority — or once
//!   *every* eligible voter has voted without a NO-majority — the deletion
//!   **applies** (the fact is tombstoned). The all-voted early resolution fires
//!   here in [`cast_vote`] ([`VoteOutcome::Applied`]); the silent-deadline path
//!   is driven by the overdue sweep
//!   ([`crate::proposals::auto_apply_overdue_proposals`], straight to `applied`).
//!
//! Votes are **final** (one row per voter, [`VoteError::AlreadyVoted`] on a
//! re-vote), reusing the `structure_proposal_votes` table. A vote-resolved
//! forget is final — there is no revert lever; the only undo is re-stating the
//! fact.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::fact_index::{self, FactIndexError};
use crate::proposals::{self, EmitParams, REVERT_WINDOW, kind};
use crate::types::{FactId, Principal};
use crate::wiki::WikiTree;

/// A cast vote's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vote {
    /// Approve the forget (consent — same effect as staying silent, but
    /// recorded so the all-voted early resolution can fire).
    Yes,
    /// Reject the forget; enough of these block the request (the fact stays).
    No,
}

impl Vote {
    /// Wire string stored in the `vote` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}

/// Outcome of [`cast_vote`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum VoteOutcome {
    /// Vote recorded; the request is still pending (more votes may arrive, or
    /// the window may close as silent consent).
    Recorded {
        /// NO votes so far.
        no_votes: i64,
        /// YES votes so far.
        yes_votes: i64,
        /// Eligible voter count (the denominator).
        eligible: i64,
    },
    /// This vote tipped a NO majority → the request is **blocked** (the proposal
    /// is expired, the fact stays active).
    Rejected {
        /// NO votes that carried the block.
        no_votes: i64,
        /// Eligible voter count.
        eligible: i64,
    },
    /// Every eligible voter has now voted without a NO majority → the forget
    /// **applies now** (the fact is tombstoned).
    Applied {
        /// Final NO tally.
        no_votes: i64,
        /// Final YES tally.
        yes_votes: i64,
        /// Eligible voter count.
        eligible: i64,
    },
}

/// Errors raised by [`cast_vote`].
#[derive(Debug, thiserror::Error)]
pub enum VoteError {
    /// No `structure_proposals` row with this id.
    #[error("proposal not found: {0}")]
    NotFound(String),
    /// The proposal is not a votable pending `fact_forget` request (wrong kind,
    /// or no recorded eligible set).
    #[error("proposal {0} is not a votable fact-forget request")]
    NotVotable(String),
    /// `voter` is not in the request's eligible set (not in the audience, or the
    /// requester, who already consented by asking).
    #[error("voter {voter} is not eligible to vote on {proposal_id}")]
    NotEligible {
        /// The proposal id.
        proposal_id: String,
        /// The refused voter.
        voter: String,
    },
    /// Voting is closed: the request is no longer `pending` (already resolved —
    /// applied, rejected, or swept past its deadline).
    #[error("voting on {0} is closed (already resolved, or the window elapsed)")]
    Closed(String),
    /// `voter` has already cast a (final) vote on this request.
    #[error("voter {voter} has already voted on {proposal_id}")]
    AlreadyVoted {
        /// The proposal id.
        proposal_id: String,
        /// The repeat voter.
        voter: String,
    },
    /// Underlying SQL failure.
    #[error("vote db: {0}")]
    Db(#[from] sqlx::Error),
    /// The proposal `context` column was not the expected JSON shape.
    #[error("vote json: {0}")]
    Json(#[from] serde_json::Error),
    /// Applying the forget (on an all-voted quorum) failed.
    #[error("forget apply: {0}")]
    Apply(#[from] proposals::ApplyError),
}

/// One eligible voter casts a final vote on a pending `fact_forget` request,
/// then the tally runs.
///
/// On a NO majority (`no * 2 > eligible`) the request is **blocked** — the
/// proposal is expired and the fact stays ([`VoteOutcome::Rejected`]); once
/// everyone has voted without a majority the forget **applies now** (the fact is
/// tombstoned, [`VoteOutcome::Applied`]); otherwise [`VoteOutcome::Recorded`].
///
/// # Errors
///
/// [`VoteError`] for a missing / non-votable / closed request, an ineligible or
/// repeat voter, a DB / JSON failure, or an apply failure.
pub async fn cast_vote(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    proposal_id: &str,
    voter_id: &str,
    vote: Vote,
) -> Result<VoteOutcome, VoteError> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT kind, status, context, timeout_at FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    let (kind_str, status, context_raw, timeout_at) =
        row.ok_or_else(|| VoteError::NotFound(proposal_id.to_owned()))?;

    // Must be a fact_forget request (has an eligible-voter set).
    if kind_str != kind::FACT_FORGET {
        return Err(VoteError::NotVotable(proposal_id.to_owned()));
    }
    let context: Value = serde_json::from_str(&context_raw)?;
    let eligible: Vec<String> = context
        .get("eligible_voters")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if eligible.is_empty() {
        return Err(VoteError::NotVotable(proposal_id.to_owned()));
    }

    // The window must still be open: the request is still `pending` (a resolved
    // — applied/rejected/swept — request is closed to votes).
    if status != "pending" {
        return Err(VoteError::Closed(proposal_id.to_owned()));
    }

    // ...and still open by the clock. Once the deadline has passed, silence =
    // consent has already carried (the overdue sweep will apply the forget), so
    // a late NO must not retro-block it: refuse the vote as closed. The status
    // check above only sees `pending` until the sweep runs, so without this a
    // post-deadline NO arriving before the sweep would wrongly count. The sweep
    // self-heals the tally either way; this just keeps the clock authoritative.
    if let Ok(deadline) = chrono::DateTime::parse_from_rfc3339(&timeout_at)
        && chrono::Utc::now() >= deadline
    {
        return Err(VoteError::Closed(proposal_id.to_owned()));
    }

    if !eligible.iter().any(|m| m == voter_id) {
        return Err(VoteError::NotEligible {
            proposal_id: proposal_id.to_owned(),
            voter: voter_id.to_owned(),
        });
    }

    // Votes are final: refuse a re-vote.
    let already: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM structure_proposal_votes WHERE proposal_id = ? AND voter_id = ?",
    )
    .bind(proposal_id)
    .bind(voter_id)
    .fetch_one(pool)
    .await?;
    if already > 0 {
        return Err(VoteError::AlreadyVoted {
            proposal_id: proposal_id.to_owned(),
            voter: voter_id.to_owned(),
        });
    }
    sqlx::query(
        "INSERT INTO structure_proposal_votes (proposal_id, voter_id, vote, voted_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(proposal_id)
    .bind(voter_id)
    .bind(vote.as_str())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await?;

    // Tally.
    let (no_votes, yes_votes) = tally(pool, proposal_id).await?;
    let eligible_n = i64::try_from(eligible.len()).unwrap_or(i64::MAX);

    if no_votes * 2 > eligible_n {
        // NO majority → block the request: expire the pending proposal (the fact
        // stays active — it was never removed). A concurrent vote may have
        // resolved it first; the conditional UPDATE makes that a 0-row no-op.
        proposals::expire_pending_proposal(pool, proposal_id).await?;
        return Ok(VoteOutcome::Rejected {
            no_votes,
            eligible: eligible_n,
        });
    }

    if no_votes + yes_votes >= eligible_n {
        // Everyone voted, no NO-majority → consent → apply the forget now
        // (tombstone the fact, proposal → applied). Idempotent against a
        // concurrent resolution: apply_proposal refuses a non-pending row.
        match proposals::apply_fact_forget_now(pool, tree, proposal_id).await {
            Ok(())
            | Err(proposals::ApplyError::NotPending { .. } | proposals::ApplyError::NotFound(_)) =>
                {},
            Err(e) => return Err(VoteError::Apply(e)),
        }
        strip_forgotten_fact_region(pool, tree, embedder, &context, proposal_id).await;
        return Ok(VoteOutcome::Applied {
            no_votes,
            yes_votes,
            eligible: eligible_n,
        });
    }

    Ok(VoteOutcome::Recorded {
        no_votes,
        yes_votes,
        eligible: eligible_n,
    })
}

/// Disk half of a vote-resolved forget: excise the tombstoned fact's
/// on-disk region ([`crate::reindex::strip_fact_region`]).
///
/// Best-effort — the strip only ever touches a RETIRED row, so if a
/// concurrent resolution left the fact active it is a no-op; a failure
/// leaves fail-closed-redacted residue for the light-dream hygiene sweep.
async fn strip_forgotten_fact_region(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    context: &Value,
    proposal_id: &str,
) {
    let Some(fact_id) = context
        .get("fact_id")
        .and_then(Value::as_str)
        .and_then(|s| FactId::parse(s).ok())
    else {
        return;
    };
    if let Err(e) = crate::reindex::strip_fact_region(pool, tree, embedder.clone(), &fact_id).await
    {
        tracing::warn!(
            proposal_id,
            fact_id = fact_id.as_str(),
            error = %e,
            "fact_forget vote: page-strip failed (redaction still applies)"
        );
    }
}

/// Outcome of [`open_forget_request`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ForgetRequest {
    /// A vote was opened: the fact stays active until the window closes. The
    /// requester's agent surfaces the deadline; the audience votes via the
    /// `pending_votes` reminder.
    VoteOpened {
        /// The pending `fact_forget` proposal id.
        proposal_id: String,
        /// The fact under request.
        fact_id: String,
        /// Eligible voter ids — the fact's audience minus the requester.
        eligible_voters: Vec<String>,
        /// RFC 3339 voting deadline (`now + REVERT_WINDOW`): silence past it is
        /// consent (the forget then applies).
        deadline: String,
    },
    /// The requester was the fact's **only** reader (the audience minus them is
    /// empty), so there is nobody to vote — the forget applied immediately (the
    /// fact was tombstoned).
    AppliedImmediately {
        /// The forgotten fact.
        fact_id: String,
    },
}

/// Errors raised by [`open_forget_request`].
#[derive(Debug, thiserror::Error)]
pub enum ForgetRequestError {
    /// No `fact_index` row with this id.
    #[error("fact not found: {0}")]
    FactNotFound(String),
    /// The fact is already tombstoned — nothing to forget.
    #[error("fact {0} is already forgotten")]
    AlreadyForgotten(String),
    /// The requester **is** the fact's sender (author): they delete it directly
    /// via the sender-direct path ([`crate::acl::can_delete`]), no vote needed.
    #[error("requester {requester} authored fact {fact_id} — delete it directly, no vote")]
    SenderActsDirectly {
        /// The fact id.
        fact_id: String,
        /// The requester.
        requester: String,
    },
    /// The requester is not authorized to open a forget request: not an admin,
    /// not the fact's `owner` (subject), and not a member of an owning group.
    #[error("requester {requester} not authorized to request forgetting {fact_id}")]
    NotAuthorized {
        /// The fact id.
        fact_id: String,
        /// The requester.
        requester: String,
    },
    /// Underlying SQL failure.
    #[error("forget request db: {0}")]
    Db(#[from] sqlx::Error),
    /// A `fact_index` read or tombstone failed.
    #[error(transparent)]
    FactIndex(#[from] FactIndexError),
    /// Emitting the pending proposal failed.
    #[error("forget request emit: {0}")]
    Emit(#[from] proposals::ProposalsError),
}

/// Open a **forget request** for `fact_id` on behalf of `requester` (a bare user
/// id), the non-sender owner's path (module docs).
///
/// Refuses when the fact is missing / already tombstoned, when the requester is
/// the fact's **sender** (they delete directly), or when the requester is not
/// authorized to request (authorized = `is_admin`, OR `owner == user:requester`,
/// OR `owner == group:g` with the requester a member of `g`). Computes the
/// eligible audience ([`crate::acl::audience`]) minus the requester; if that is
/// **empty** (the requester is the fact's only reader) the forget applies
/// immediately. Otherwise a **pending** `fact_forget` proposal is emitted with a
/// [`REVERT_WINDOW`] voting deadline, and the proposal id + eligible set +
/// deadline are returned.
///
/// # Errors
///
/// [`ForgetRequestError`] for any of the refusals above or a DB / emit failure.
pub async fn open_forget_request(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    fact_id: &FactId,
    requester: &str,
    is_admin: bool,
) -> Result<ForgetRequest, ForgetRequestError> {
    let row = fact_index::find_by_id(pool, fact_id)
        .await?
        .ok_or_else(|| ForgetRequestError::FactNotFound(fact_id.as_str().to_owned()))?;
    if row.deleted_at.is_some() {
        return Err(ForgetRequestError::AlreadyForgotten(
            fact_id.as_str().to_owned(),
        ));
    }

    // A sender deletes their own contribution directly (3·fact) — no vote.
    if matches!(&row.sender_id, Some(Principal::User(s)) if s == requester) {
        return Err(ForgetRequestError::SenderActsDirectly {
            fact_id: fact_id.as_str().to_owned(),
            requester: requester.to_owned(),
        });
    }

    // Only the owner (subject) or an owning-group member — or an admin — may
    // open the request.
    if !is_admin && !owner_authorizes(pool, &row.owner_id, requester).await? {
        return Err(ForgetRequestError::NotAuthorized {
            fact_id: fact_id.as_str().to_owned(),
            requester: requester.to_owned(),
        });
    }

    // The electorate: the fact's read audience minus the requester (who
    // consented by asking).
    let mut eligible =
        crate::acl::audience(pool, &row.owner_id, &row.allow_ids, row.sender_id.as_ref()).await?;
    eligible.retain(|u| u != requester);

    // Nobody left to vote (the requester was the only reader) → apply now.
    if eligible.is_empty() {
        fact_index::mark_forgotten(pool, fact_id, "fact_forget_sole_reader").await?;
        // Disk half: excise the tombstoned region's bytes (best-effort —
        // residue redacts fail-closed and the hygiene sweep picks it up).
        if let Err(e) =
            crate::reindex::strip_fact_region(pool, tree, embedder.clone(), fact_id).await
        {
            tracing::warn!(
                fact_id = fact_id.as_str(),
                error = %e,
                "fact_forget sole-reader: page-strip failed (redaction still applies)"
            );
        }
        return Ok(ForgetRequest::AppliedImmediately {
            fact_id: fact_id.as_str().to_owned(),
        });
    }

    // Open a pending vote: the fact stays active until the window closes.
    let context = serde_json::json!({
        "variant": "fact_forget",
        "fact_id": fact_id.as_str(),
        "requester": requester,
        "eligible_voters": eligible,
    });
    let proposal_id = proposals::emit_proposal(
        pool,
        EmitParams::new(kind::FACT_FORGET, context, forget_questions(fact_id))
            .with_timeout(REVERT_WINDOW)
            .with_recipient(Some(format!("user:{requester}"))),
    )
    .await?;

    let deadline = (chrono::Utc::now() + REVERT_WINDOW).to_rfc3339();
    Ok(ForgetRequest::VoteOpened {
        proposal_id,
        fact_id: fact_id.as_str().to_owned(),
        eligible_voters: eligible,
        deadline,
    })
}

/// Whether `requester` is the fact's `owner` (subject) or a member of an owning
/// group — the request-authorization predicate (the sender / admin paths are
/// handled by the caller). The builtin `global` group authorizes no one (a
/// public fact has no specific owner to request on its behalf).
async fn owner_authorizes(
    pool: &SqlitePool,
    owner: &Principal,
    requester: &str,
) -> Result<bool, sqlx::Error> {
    match owner {
        Principal::User(id) => Ok(id == requester),
        Principal::Group(id) if id == "global" => Ok(false),
        Principal::Group(id) => Ok(crate::enrollment::members_for(pool, id)
            .await?
            .iter()
            .any(|m| m == requester)),
    }
}

/// Display-only questionnaire stored on the pending `fact_forget` proposal (the
/// dashboard renders it; the resolution is the audience vote / silence sweep).
fn forget_questions(fact_id: &FactId) -> Value {
    serde_json::json!([{
        "id": "fact_forget",
        "text": format!(
            "A forget request was opened for fact `{}`. Silence is consent (it will be \
             forgotten when the window closes); a NO majority of the audience blocks it.",
            fact_id.as_str()
        ),
        "options": [
            { "id": "forget", "recommended": true },
            { "id": "keep" },
        ],
    }])
}

/// A pending `fact_forget` request this voter still owes a vote on — the payload
/// of the recall reminder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingVote {
    /// The `fact_forget` proposal to vote on.
    pub proposal_id: String,
    /// The fact under request.
    pub fact_id: String,
    /// The user who opened the request (the fact's owner / an owning-group
    /// member).
    pub requester: String,
    /// RFC 3339 deadline — silence past it is consent (the forget applies).
    pub deadline: String,
    /// Relative dashboard path that lands the member in the chat with this
    /// request summarised, where they cast the vote (`structure_proposal_vote`).
    /// The consumer agent prefixes it with the operator's base URL and hands it
    /// to the human.
    pub dashboard_path: String,
}

/// The pending forget requests `voter_id` is eligible for and has not yet voted
/// on, within their open windows.
///
/// The pull-only reminder (fact-forget vote):
/// surfaced in a member's recall the next time they interact, never pushed. A
/// member who never looks consents by silence when the window closes.
///
/// # Errors
///
/// [`VoteError::Db`] for a SQL failure (a malformed `context` blob is skipped,
/// not fatal — one bad row never hides the others).
pub async fn pending_votes_for(
    pool: &SqlitePool,
    voter_id: &str,
) -> Result<Vec<PendingVote>, VoteError> {
    if voter_id.is_empty() {
        return Ok(Vec::new());
    }
    let now = chrono::Utc::now().to_rfc3339();
    // Open (pending) fact_forget requests the member has not voted on yet;
    // eligibility + details come from the context JSON, parsed below.
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT proposal_id, context, timeout_at
           FROM structure_proposals
          WHERE kind = ? AND status = 'pending'
            AND timeout_at > ?
            AND proposal_id NOT IN (
                SELECT proposal_id FROM structure_proposal_votes WHERE voter_id = ?
            )
          ORDER BY timeout_at ASC",
    )
    .bind(kind::FACT_FORGET)
    .bind(&now)
    .bind(voter_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for (proposal_id, context_raw, deadline) in rows {
        let Ok(context) = serde_json::from_str::<Value>(&context_raw) else {
            continue; // a malformed row never hides the rest
        };
        let eligible = context
            .get("eligible_voters")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(voter_id)));
        if !eligible {
            continue;
        }
        out.push(PendingVote {
            dashboard_path: proposals::proposal_dashboard_path(&proposal_id),
            proposal_id,
            fact_id: context
                .get("fact_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            requester: context
                .get("requester")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            deadline,
        });
    }
    Ok(out)
}

/// Count the (no, yes) votes on a proposal.
async fn tally(pool: &SqlitePool, proposal_id: &str) -> Result<(i64, i64), sqlx::Error> {
    let no: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM structure_proposal_votes WHERE proposal_id = ? AND vote = 'no'",
    )
    .bind(proposal_id)
    .fetch_one(pool)
    .await?;
    let yes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM structure_proposal_votes WHERE proposal_id = ? AND vote = 'yes'",
    )
    .bind(proposal_id)
    .fetch_one(pool)
    .await?;
    Ok((no, yes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::enrollment::{self, EnrollmentFile, GroupEntry, UserEntry};
    use crate::fact_index::{self, NewFact};
    use crate::wiki::WikiTree;

    const UUID1: &str = "0190a0c8-0000-7000-8000-000000000001";

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("fake", 4))
    }

    fn uid(id: &str) -> UserEntry {
        UserEntry {
            id: id.into(),
            aliases: Vec::new(),
            is_admin: false,
            locale: None,
            timezone: None,
        }
    }

    /// Open a (pool, tree) with famiglia = {franz, morgana, bilbo} enrolled and a
    /// `famiglia` group wiki on disk (so a group-owned fact resolves).
    async fn seed_famiglia() -> (SqlitePool, WikiTree) {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        let tree =
            WikiTree::open(Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path())
                .expect("tree");
        let dir = tree.wikis_dir().join("famiglia");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("_meta.md"),
            "---\nwiki_id: famiglia\nwiki_type: wiki-group\nparent_wiki_id: null\n\
             slug: famiglia\ntitle: famiglia\nacl_default: 'group:famiglia'\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(tree.workdir()).expect("reopen tree");

        enrollment::mirror_to_db(
            &pool,
            &EnrollmentFile {
                version: 1,
                users: vec![uid("franz"), uid("morgana"), uid("bilbo")],
                groups: vec![GroupEntry {
                    id: "famiglia".into(),
                    members: vec!["franz".into(), "morgana".into(), "bilbo".into()],
                    scope: None,
                }],
            },
        )
        .await
        .expect("mirror enrollment");
        (pool, tree)
    }

    /// Insert one fact on `famiglia/vacanze.md` with explicit owner/allow/sender.
    async fn insert_fact(
        pool: &SqlitePool,
        owner: &str,
        allow: &[&str],
        sender: Option<&str>,
    ) -> FactId {
        let fact_id = FactId::parse(UUID1).unwrap();
        fact_index::insert(
            pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "famiglia".to_owned(),
                source_path: "wikis/famiglia/vacanze.md".to_owned(),
                region_start: Some(0),
                region_end: Some(32),
                text: "shared family fact".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                owner_id: owner.parse().unwrap(),
                allow_ids: allow.iter().map(|a| a.parse().unwrap()).collect(),
                sender_id: sender.map(|s| s.parse().unwrap()),
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .expect("insert fact");
        fact_id
    }

    async fn is_tombstoned(pool: &SqlitePool, fact_id: &FactId) -> bool {
        fact_index::find_by_id(pool, fact_id)
            .await
            .expect("find")
            .expect("row")
            .deleted_at
            .is_some()
    }

    async fn status_of(pool: &SqlitePool, proposal_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
            .bind(proposal_id)
            .fetch_one(pool)
            .await
            .expect("status")
    }

    /// franz owns the fact morgana authored: franz (owner, NOT sender) opens a
    /// request; the audience minus franz is {bilbo, morgana}.
    fn opened_voters(req: &ForgetRequest) -> (&str, &[String]) {
        match req {
            ForgetRequest::VoteOpened {
                proposal_id,
                eligible_voters,
                ..
            } => (proposal_id, eligible_voters),
            ForgetRequest::AppliedImmediately { .. } => {
                panic!("expected a vote to open, got immediate apply")
            },
        }
    }

    #[tokio::test]
    async fn owner_opens_request_audience_all_yes_applies() {
        let (pool, tree) = seed_famiglia().await;
        // owner=franz, allow=famiglia, sender=morgana → franz is owner not sender.
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;

        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, voters) = opened_voters(&req);
        assert_eq!(voters, &["bilbo", "morgana"], "audience minus requester");
        assert!(
            !is_tombstoned(&pool, &fact).await,
            "fact stays during the vote"
        );

        // bilbo YES — recorded, still pending (1 of 2 voted, no NO-majority).
        let r = cast_vote(&pool, &tree, &embedder(), proposal_id, "bilbo", Vote::Yes)
            .await
            .expect("bilbo votes");
        assert!(matches!(r, VoteOutcome::Recorded { eligible: 2, .. }));
        assert!(!is_tombstoned(&pool, &fact).await);

        // morgana YES — all voted, no NO-majority → applies now.
        let r = cast_vote(&pool, &tree, &embedder(), proposal_id, "morgana", Vote::Yes)
            .await
            .expect("morgana votes");
        assert!(matches!(
            r,
            VoteOutcome::Applied {
                no_votes: 0,
                yes_votes: 2,
                eligible: 2
            }
        ));
        assert!(
            is_tombstoned(&pool, &fact).await,
            "all-yes tombstoned the fact"
        );
        assert_eq!(status_of(&pool, proposal_id).await, "applied");
    }

    #[tokio::test]
    async fn no_majority_blocks_request_fact_stays() {
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);

        // bilbo NO — 1 of 2, not a majority yet.
        let r = cast_vote(&pool, &tree, &embedder(), proposal_id, "bilbo", Vote::No)
            .await
            .expect("bilbo NO");
        assert!(matches!(r, VoteOutcome::Recorded { no_votes: 1, .. }));
        assert!(!is_tombstoned(&pool, &fact).await);

        // morgana NO — 2 of 2 → NO-majority → blocked, fact stays.
        let r = cast_vote(&pool, &tree, &embedder(), proposal_id, "morgana", Vote::No)
            .await
            .expect("morgana NO");
        assert!(matches!(
            r,
            VoteOutcome::Rejected {
                no_votes: 2,
                eligible: 2
            }
        ));
        assert!(
            !is_tombstoned(&pool, &fact).await,
            "the fact survives the block"
        );
        assert_eq!(status_of(&pool, proposal_id).await, "expired");

        // Voting is closed now.
        let err = cast_vote(&pool, &tree, &embedder(), proposal_id, "bilbo", Vote::Yes)
            .await
            .expect_err("closed");
        assert!(matches!(
            err,
            VoteError::Closed(_) | VoteError::AlreadyVoted { .. }
        ));
    }

    #[tokio::test]
    async fn single_reader_request_applies_immediately() {
        let (pool, tree) = seed_famiglia().await;
        // owner=franz, no allow, no sender → franz is the only reader.
        let fact = insert_fact(&pool, "user:franz", &[], None).await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        assert!(
            matches!(req, ForgetRequest::AppliedImmediately { .. }),
            "no electorate → apply now"
        );
        assert!(is_tombstoned(&pool, &fact).await, "tombstoned immediately");
    }

    #[tokio::test]
    async fn sender_cannot_open_a_request() {
        let (pool, tree) = seed_famiglia().await;
        // morgana is the sender — they delete directly, not via a vote.
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let err = open_forget_request(&pool, &tree, &embedder(), &fact, "morgana", false)
            .await
            .expect_err("sender refused");
        assert!(matches!(err, ForgetRequestError::SenderActsDirectly { .. }));
        assert!(!is_tombstoned(&pool, &fact).await);
    }

    #[tokio::test]
    async fn unauthorized_requester_is_refused() {
        let (pool, tree) = seed_famiglia().await;
        // owner=franz (a user), sender=morgana. bilbo is neither owner nor sender
        // and not an admin → refused (he is in the audience but cannot *request*).
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let err = open_forget_request(&pool, &tree, &embedder(), &fact, "bilbo", false)
            .await
            .expect_err("non-owner refused");
        assert!(matches!(err, ForgetRequestError::NotAuthorized { .. }));
    }

    #[tokio::test]
    async fn group_owner_member_may_request() {
        let (pool, tree) = seed_famiglia().await;
        // owner=group:famiglia, sender=morgana. bilbo (a famiglia member, not the
        // sender) may open the request; audience minus bilbo = {franz, morgana}.
        let fact = insert_fact(&pool, "group:famiglia", &[], Some("user:morgana")).await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "bilbo", false)
            .await
            .expect("group member requests");
        let (_, voters) = opened_voters(&req);
        assert_eq!(voters, &["franz", "morgana"]);
    }

    #[tokio::test]
    async fn admin_may_request_even_when_not_owner() {
        let (pool, tree) = seed_famiglia().await;
        // owner=franz, sender=morgana. nina is neither, but as admin may request.
        // Audience = {franz, morgana}; minus requester nina (not in it) = both.
        let fact = insert_fact(&pool, "user:franz", &[], Some("user:morgana")).await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "nina", true)
            .await
            .expect("admin requests");
        let (_, voters) = opened_voters(&req);
        assert_eq!(voters, &["franz", "morgana"]);
    }

    #[tokio::test]
    async fn silence_past_deadline_applies_via_sweep() {
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);
        let proposal_id = proposal_id.to_owned();

        // Nobody votes. Backdate the voting deadline (timeout_at) into the past.
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET timeout_at = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !is_tombstoned(&pool, &fact).await,
            "still active before the sweep"
        );

        // The overdue sweep resolves silence = consent → applied (tombstoned).
        let report = proposals::auto_apply_overdue_proposals(&pool, &tree, chrono::Utc::now())
            .await
            .expect("sweep");
        assert!(
            report
                .auto_applied
                .iter()
                .any(|(id, k)| id == &proposal_id && k == kind::FACT_FORGET),
            "the sweep applied the overdue fact_forget"
        );
        assert!(
            is_tombstoned(&pool, &fact).await,
            "silence applied the forget"
        );
        assert_eq!(status_of(&pool, &proposal_id).await, "applied");
    }

    #[tokio::test]
    async fn vote_after_deadline_is_closed_not_counted() {
        // A NO arriving after the deadline (but before the overdue sweep flips
        // the status off `pending`) must be refused as closed — silence =
        // consent has already carried, so a late NO cannot retro-block.
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);
        let proposal_id = proposal_id.to_owned();

        // Backdate the deadline; the proposal is still `pending` (no sweep yet).
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET timeout_at = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();

        // A late NO is refused as closed and never recorded.
        let err = cast_vote(&pool, &tree, &embedder(), &proposal_id, "bilbo", Vote::No)
            .await
            .expect_err("late vote refused");
        assert!(matches!(err, VoteError::Closed(_)));
        let (no_votes, _) = tally(&pool, &proposal_id).await.expect("tally");
        assert_eq!(no_votes, 0, "the late NO was not recorded");
        assert!(!is_tombstoned(&pool, &fact).await, "fact still active");
    }

    #[tokio::test]
    async fn recorded_no_majority_at_deadline_expires_not_applies() {
        // A single NO on a 2-voter electorate is not a NO-majority, so the
        // request stays pending; but if it somehow reaches the deadline with a
        // recorded NO-majority the sweep must expire it, never apply it.
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);
        let proposal_id = proposal_id.to_owned();

        // Inject a NO-majority directly (2 NOs of 2 eligible) without going
        // through cast_vote (which would have expired it live).
        for voter in ["bilbo", "morgana"] {
            sqlx::query(
                "INSERT INTO structure_proposal_votes (proposal_id, voter_id, vote, voted_at)
                 VALUES (?, ?, 'no', ?)",
            )
            .bind(&proposal_id)
            .bind(voter)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(&pool)
            .await
            .unwrap();
        }
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET timeout_at = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();

        proposals::auto_apply_overdue_proposals(&pool, &tree, chrono::Utc::now())
            .await
            .expect("sweep");
        assert!(
            !is_tombstoned(&pool, &fact).await,
            "NO-majority blocks even at deadline"
        );
        assert_eq!(status_of(&pool, &proposal_id).await, "expired");
    }

    #[tokio::test]
    async fn pending_votes_surface_to_eligible_unvoted_not_requester() {
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);
        let proposal_id = proposal_id.to_owned();

        // bilbo + morgana owe a vote; the requester franz does not.
        let bilbo = pending_votes_for(&pool, "bilbo").await.expect("bilbo");
        assert_eq!(bilbo.len(), 1);
        assert_eq!(bilbo[0].proposal_id, proposal_id);
        assert_eq!(bilbo[0].fact_id, fact.as_str());
        assert_eq!(bilbo[0].requester, "franz");
        assert_eq!(
            pending_votes_for(&pool, "morgana").await.expect("x").len(),
            1
        );
        assert!(
            pending_votes_for(&pool, "franz")
                .await
                .expect("f")
                .is_empty(),
            "the requester is not a voter"
        );

        // Once bilbo votes, the reminder stops surfacing to him.
        cast_vote(&pool, &tree, &embedder(), &proposal_id, "bilbo", Vote::Yes)
            .await
            .expect("bilbo votes");
        assert!(
            pending_votes_for(&pool, "bilbo")
                .await
                .expect("b2")
                .is_empty(),
            "a voted member no longer owes a vote"
        );
    }

    #[tokio::test]
    async fn rejects_ineligible_repeat_and_non_fact_forget_votes() {
        let (pool, tree) = seed_famiglia().await;
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        let req = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect("open request");
        let (proposal_id, _) = opened_voters(&req);
        let proposal_id = proposal_id.to_owned();

        // The requester is not an eligible voter (consented by asking).
        let err = cast_vote(&pool, &tree, &embedder(), &proposal_id, "franz", Vote::No)
            .await
            .expect_err("requester cannot vote");
        assert!(matches!(err, VoteError::NotEligible { .. }));

        // A non-audience user cannot vote.
        let err = cast_vote(
            &pool,
            &tree,
            &embedder(),
            &proposal_id,
            "stranger",
            Vote::No,
        )
        .await
        .expect_err("non-audience");
        assert!(matches!(err, VoteError::NotEligible { .. }));

        // A repeat vote is refused (votes are final).
        cast_vote(&pool, &tree, &embedder(), &proposal_id, "bilbo", Vote::Yes)
            .await
            .expect("bilbo first vote");
        let err = cast_vote(&pool, &tree, &embedder(), &proposal_id, "bilbo", Vote::No)
            .await
            .expect_err("re-vote");
        assert!(matches!(err, VoteError::AlreadyVoted { .. }));

        // An unknown proposal id is NotFound.
        let err = cast_vote(&pool, &tree, &embedder(), "no-such-id", "bilbo", Vote::No)
            .await
            .expect_err("missing");
        assert!(matches!(err, VoteError::NotFound(_)));

        // A non-fact_forget proposal is NotVotable.
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status) VALUES ('bundle-x', 'bundle', '{}', '[]', ?, ?, 'applied')",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();
        let err = cast_vote(&pool, &tree, &embedder(), "bundle-x", "bilbo", Vote::No)
            .await
            .expect_err("non fact_forget");
        assert!(matches!(err, VoteError::NotVotable(_)));
    }

    #[tokio::test]
    async fn open_request_refuses_missing_and_tombstoned_facts() {
        let (pool, tree) = seed_famiglia().await;
        // Missing fact.
        let missing = FactId::parse("0190a0c8-0000-7000-8000-0000000000ff").unwrap();
        let err = open_forget_request(&pool, &tree, &embedder(), &missing, "franz", true)
            .await
            .expect_err("missing");
        assert!(matches!(err, ForgetRequestError::FactNotFound(_)));

        // Already-tombstoned fact.
        let fact = insert_fact(
            &pool,
            "user:franz",
            &["group:famiglia"],
            Some("user:morgana"),
        )
        .await;
        fact_index::mark_forgotten(&pool, &fact, "test")
            .await
            .expect("forget");
        let err = open_forget_request(&pool, &tree, &embedder(), &fact, "franz", false)
            .await
            .expect_err("already forgotten");
        assert!(matches!(err, ForgetRequestError::AlreadyForgotten(_)));
    }
}
