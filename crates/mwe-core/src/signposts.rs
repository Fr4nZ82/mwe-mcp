// SPDX-License-Identifier: AGPL-3.0-or-later
//! Project **signposts** — the standard agent learns that a project exists.
//!
//! A conversational turn recalls facts, never smart-wiki sections: that is
//! what keeps a personal exchange from being buried under project
//! documentation ([`crate::sections`]). The price is that a project the
//! user never *names* is invisible to their standard agent — the memory
//! cannot connect a dot it cannot see.
//!
//! A signpost is the dot. It is a short fact, in the owner's own standard
//! wiki, on the reserved page [`crate::wiki::PROJECTS_FILENAME`], written
//! by the smart consumer that maintains the project:
//!
//! - **one description per project** — what it is and what it is for, in
//!   plain language, [`MAX_DESCRIPTION_CHARS`] at most;
//! - **one activity line per project per day** — what happened that day,
//!   [`MAX_ACTIVITY_CHARS`] at most, kept for [`ACTIVITY_WINDOW_DAYS`].
//!
//! ## A signpost is not a record
//!
//! It exists to make the engine aware that the project is there and to
//! *cause the deepening* — a surfaced signpost opens its project's
//! sections in the same turn
//! ([`crate::recall::recall_project_docs`]). What was actually
//! done lives in the project's own wiki, and answering from the signpost
//! would be answering from a summary of a summary. Hence the short caps
//! and the rolling window: they are not storage limits, they bound how
//! much of a *turn's* context one project may take before the agent has
//! decided the project is even relevant.
//!
//! ## Deterministic on purpose
//!
//! The ordinary capture path would run the ingest classifier, which
//! decides placement — and placement is exactly what must be guaranteed
//! here. So this channel writes straight through
//! [`crate::capture::wiki_capture`] with dedup off: a signpost's identity
//! is its **topic key** (project, and day for an activity line), not its
//! similarity to its neighbours. Two projects described in similar words
//! stay two signposts.
//!
//! The caps are enforced here rather than asked for in the skill: a limit
//! politely requested of an LLM is not a limit. Over-long input is
//! refused with the measured length, never truncated — a signpost cut
//! mid-sentence would be quoted verbatim into a recall block.
//!
//! Re-writing an unchanged signpost is a **no-op**, so the smart consumer
//! can refresh on every push without churning the page, the embeddings,
//! or the recall counters.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::NaiveDate;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture::{self, CaptureAction, CaptureError, CaptureRequest};
use crate::embedder::Embedder;
use crate::fact_index::{self, FactIndexError, FactIndexRow};
use crate::types::{FactId, Principal, WikiId};
use crate::wiki::{WikiError, WikiTree};

/// Character cap on a project description.
pub const MAX_DESCRIPTION_CHARS: usize = 400;

/// Character cap on one day's activity line.
pub const MAX_ACTIVITY_CHARS: usize = 250;

/// How many calendar days of activity lines a project keeps, counting the
/// most recent day written. Older lines are tombstoned on the next write.
pub const ACTIVITY_WINDOW_DAYS: i64 = 5;

/// `fact_type` stamped on every signpost row.
pub const SIGNPOST_FACT_TYPE: &str = "signpost";

/// Topic marking a fact as a signpost. Present on both kinds.
pub const TOPIC_SIGNPOST: &str = "project-signpost";

/// Topic marking the description signpost of a project.
const TOPIC_DESCRIPTION: &str = "signpost-description";

/// Topic prefix carrying the project wiki id a signpost points at.
const TOPIC_WIKI_PREFIX: &str = "signpost-wiki:";

/// Topic prefix carrying an activity line's day (`YYYY-MM-DD`).
const TOPIC_DAY_PREFIX: &str = "signpost-day:";

/// Errors raised by the signpost channel.
#[derive(Debug, Error)]
pub enum SignpostError {
    /// Underlying wiki-tree error.
    #[error("signpost wiki: {0}")]
    Wiki(#[from] WikiError),

    /// Underlying capture error.
    #[error("signpost capture: {0}")]
    Capture(#[from] CaptureError),

    /// Underlying fact-index error.
    #[error("signpost fact_index: {0}")]
    FactIndex(#[from] FactIndexError),

    /// Underlying `SQLite` error.
    #[error("signpost db: {0}")]
    Db(#[from] sqlx::Error),

    /// The target wiki is not a smart wiki. Signposts point at project
    /// wikis; a standard wiki needs no pointer, its facts are recalled.
    #[error("wiki {wiki_id} is not a smart wiki — signposts point at project wikis")]
    NotSmart {
        /// The offending wiki.
        wiki_id: String,
    },

    /// The caller does not own the project wiki. Same rule as
    /// `wiki_admin_push`: a smart consumer writes only its own user's
    /// wikis.
    #[error("wiki {wiki_id} belongs to {owner}, not to {caller}")]
    NotOwner {
        /// The offending wiki.
        wiki_id: String,
        /// Its owning user.
        owner: String,
        /// The calling user.
        caller: String,
    },

    /// The project wiki resolves to a group, which has no single personal
    /// wiki to signpost into.
    #[error("wiki {wiki_id} is group-owned — no personal wiki to signpost into")]
    GroupOwned {
        /// The offending wiki.
        wiki_id: String,
    },

    /// Neither a description nor an activity line was supplied.
    #[error("signpost request carries neither a description nor an activity line")]
    Empty,

    /// A field exceeded its server-enforced cap.
    #[error("{field} is {actual} characters, over the {cap}-character cap")]
    TooLong {
        /// Which field (`description` / `activity`).
        field: &'static str,
        /// The cap.
        cap: usize,
        /// What was submitted.
        actual: usize,
    },

    /// The activity day did not parse as `YYYY-MM-DD`.
    #[error("day {got:?} is not an ISO date (YYYY-MM-DD)")]
    BadDay {
        /// What was submitted.
        got: String,
    },

    /// A user id is not a usable wiki id — the owner has no standard wiki
    /// this channel could write into.
    #[error("{got:?} is not a usable wiki id: {detail}")]
    BadWikiId {
        /// What was submitted.
        got: String,
        /// The parser's complaint.
        detail: String,
    },
}

/// Parse a bare user id as its identity wiki's id.
fn owner_wiki_id(owner_user: &str) -> Result<WikiId> {
    WikiId::parse(owner_user).map_err(|e| SignpostError::BadWikiId {
        got: owner_user.to_owned(),
        detail: e.to_string(),
    })
}

type Result<T> = std::result::Result<T, SignpostError>;

/// One day's activity line for a project.
#[derive(Debug, Clone)]
pub struct ActivityLine {
    /// Calendar day the activity belongs to, `YYYY-MM-DD`.
    pub day: String,
    /// What happened, in plain language. The project name and the date
    /// are prefixed by the channel — the text carries only the substance.
    pub text: String,
}

/// A signpost write: either or both kinds, for one project.
#[derive(Debug, Clone)]
pub struct SignpostRequest {
    /// The project (smart) wiki being signposted.
    pub project_wiki_id: WikiId,
    /// Replacement description, if the caller is refreshing it.
    pub description: Option<String>,
    /// Activity line for one day, if the caller is recording one.
    pub activity: Option<ActivityLine>,
}

/// What happened to one signpost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignpostOutcome {
    /// A new signpost was written (no predecessor).
    Created(FactId),
    /// The predecessor was superseded by this one.
    Updated(FactId),
    /// The text was already exactly this — nothing was written.
    Unchanged(FactId),
}

impl SignpostOutcome {
    /// The fact id this outcome refers to.
    #[must_use]
    pub const fn fact_id(&self) -> &FactId {
        match self {
            Self::Created(id) | Self::Updated(id) | Self::Unchanged(id) => id,
        }
    }

    /// Wire word for the tool response.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::Updated(_) => "updated",
            Self::Unchanged(_) => "unchanged",
        }
    }
}

/// Outcome of one [`write`].
#[derive(Debug, Clone)]
pub struct SignpostReport {
    /// The owner's standard wiki the signposts landed in.
    pub owner_wiki_id: String,
    /// Workdir-relative path of the reserved page.
    pub source_path: String,
    /// Outcome for the description, when one was submitted.
    pub description: Option<SignpostOutcome>,
    /// Outcome for the activity line, when one was submitted.
    pub activity: Option<SignpostOutcome>,
    /// Activity lines tombstoned for falling out of the window.
    pub retired: usize,
    /// Activity lines still active for this project after the write.
    pub active_days: usize,
}

/// Write a project's signposts into its owner's reserved page.
///
/// `caller` is the bare user id from the token (`"alice"`, not
/// `"user:alice"`) — the same identity `wiki_admin_push` gates on.
///
/// # Errors
///
/// See [`SignpostError`]: an unknown / non-smart / foreign project wiki,
/// an over-long field, a malformed day, or a storage failure.
pub async fn write(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    caller: &str,
    req: SignpostRequest,
) -> Result<SignpostReport> {
    let (description, activity) = validate(&req)?;
    let target = resolve_target(tree, caller, &req.project_wiki_id)?;
    let page = PathBuf::from(crate::wiki::PROJECTS_FILENAME);

    // A project's signposts share the wiki-id topic; the page is small
    // (a handful of facts), so one scan serves every lookup below.
    let existing: Vec<FactIndexRow> =
        fact_index::find_active_by_source_path(pool, &target.source_path)
            .await?
            .into_iter()
            .filter(|row| is_signpost_for(row, req.project_wiki_id.as_str()))
            .collect();

    let mut report = SignpostReport {
        owner_wiki_id: target.owner_wiki_id.as_str().to_owned(),
        source_path: target.source_path.clone(),
        description: None,
        activity: None,
        retired: 0,
        active_days: 0,
    };

    if let Some(text) = &description {
        let body = description_body(&target.title, text);
        let previous = existing
            .iter()
            .find(|row| has_topic(row, TOPIC_DESCRIPTION));
        report.description = Some(
            put(
                pool,
                tree,
                Arc::clone(&embedder),
                PutRequest {
                    wiki_id: target.owner_wiki_id.as_str(),
                    page: &page,
                    body,
                    owner: &target.owner_principal,
                    allow: &target.allow,
                    topics: description_topics(req.project_wiki_id.as_str()),
                    previous,
                },
            )
            .await?,
        );
    }

    if let Some(line) = &activity {
        let body = activity_body(&line.day, &target.title, &line.text);
        let day_topic = format!("{TOPIC_DAY_PREFIX}{}", line.day);
        let previous = existing.iter().find(|row| has_topic(row, &day_topic));
        report.activity = Some(
            put(
                pool,
                tree,
                Arc::clone(&embedder),
                PutRequest {
                    wiki_id: target.owner_wiki_id.as_str(),
                    page: &page,
                    body,
                    owner: &target.owner_principal,
                    allow: &target.allow,
                    topics: activity_topics(req.project_wiki_id.as_str(), &line.day),
                    previous,
                },
            )
            .await?,
        );
    }

    // Roll the window. Re-read: the write above may have added a day.
    let (retired, active_days) = roll_window(
        pool,
        tree,
        embedder,
        &target.source_path,
        req.project_wiki_id.as_str(),
    )
    .await?;
    report.retired = retired;
    report.active_days = active_days;

    tracing::info!(
        project_wiki_id = req.project_wiki_id.as_str(),
        owner_wiki_id = report.owner_wiki_id,
        description = report.description.as_ref().map(SignpostOutcome::as_str),
        activity = report.activity.as_ref().map(SignpostOutcome::as_str),
        retired = report.retired,
        active_days = report.active_days,
        "signpost: written"
    );
    Ok(report)
}

/// The freshest activity day a project has on record, `YYYY-MM-DD`, or
/// `None` when it has never reported one.
///
/// # Errors
///
/// Propagates the fact-index read.
pub async fn last_activity_day(
    pool: &SqlitePool,
    source_path: &str,
    project_wiki_id: &str,
) -> Result<Option<String>> {
    Ok(fact_index::find_active_by_source_path(pool, source_path)
        .await?
        .iter()
        .filter(|row| is_signpost_for(row, project_wiki_id))
        .filter_map(day_of)
        .max())
}

/// What a project's signposts look like right now.
#[derive(Debug, Clone)]
pub struct SignpostStatus {
    /// Workdir-relative path of the owner's reserved page.
    pub page: String,
    /// Whether the project has a description signpost at all.
    pub has_description: bool,
    /// Freshest activity day on record, `YYYY-MM-DD`.
    pub last_activity_day: Option<String>,
}

/// Read a project's signpost state, for the staleness nudge
/// `wiki_admin_push` returns.
///
/// Read-only and caller-agnostic: it reports on the owner resolved from
/// the tree, so it never needs the write path's ownership gate. A wiki
/// that is not smart, whose owner is a group, or that is **not bound to
/// a project** simply has no signposts.
///
/// "Not a project" means the wiki is an **agent's own**: the consumer's
/// operational wiki, forged by the sign-in flow, is private working memory
/// and signposting it would only add noise to the owner's `projects.md` —
/// observed live on `franz-ubestia-cc`, where the nudge fired twice and
/// was correctly ignored twice. The test is deliberately that property and
/// not "has a `project_id`": `project_id` is optional on create, so a
/// project wiki pushed without one would otherwise go silently
/// undiscoverable, which is the failure this whole area exists to fix.
///
/// The property is read from the server-written `is_agent` marker, with the
/// [`crate::wiki::AGENT_WIKI_TYPE`] label kept as a fallback: that label is a
/// free-form string the *consumer* chooses on `wiki_admin_push`, so it can be
/// claimed by anything and is trustworthy only on the wikis the sign-in flow
/// wrote. Either alone would leak — the marker misses an operational wiki
/// forged before it existed and not yet re-authed, the label misses nothing
/// but can be spoofed — so the union is the honest test.
///
/// # Errors
///
/// Propagates the tree lookup and the fact-index read.
pub async fn status(
    pool: &SqlitePool,
    tree: &WikiTree,
    project_wiki_id: &WikiId,
) -> Result<Option<SignpostStatus>> {
    let project = tree.locate(project_wiki_id)?;
    if !project.meta().smart {
        return Ok(None);
    }
    if project.meta().is_agent || project.meta().wiki_type == crate::wiki::AGENT_WIKI_TYPE {
        return Ok(None);
    }
    let Principal::User(owner) = tree.resolve_scope_principal(project.meta())? else {
        return Ok(None);
    };
    let Ok(page) = page_path(tree, &owner) else {
        return Ok(None);
    };
    let rows: Vec<FactIndexRow> = fact_index::find_active_by_source_path(pool, &page)
        .await?
        .into_iter()
        .filter(|row| is_signpost_for(row, project_wiki_id.as_str()))
        .collect();
    Ok(Some(SignpostStatus {
        has_description: rows.iter().any(|row| has_topic(row, TOPIC_DESCRIPTION)),
        last_activity_day: rows.iter().filter_map(day_of).max(),
        page,
    }))
}

/// Workdir-relative path of a user's reserved signposts page.
///
/// # Errors
///
/// Propagates the wiki lookup when the user has no standard wiki.
pub fn page_path(tree: &WikiTree, owner_user: &str) -> Result<String> {
    let handle = tree.locate(&owner_wiki_id(owner_user)?)?;
    Ok(crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &handle.abs_dir().join(crate::wiki::PROJECTS_FILENAME),
    ))
}

// ---------- internals ----------

/// Trim the request and enforce the caps, before anything is resolved or
/// read: an over-long field costs no tree walk.
fn validate(req: &SignpostRequest) -> Result<(Option<String>, Option<ActivityLine>)> {
    let description = req
        .description
        .as_ref()
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());
    let activity = req
        .activity
        .as_ref()
        .map(|a| ActivityLine {
            day: a.day.trim().to_owned(),
            text: a.text.trim().to_owned(),
        })
        .filter(|a| !a.text.is_empty());
    if description.is_none() && activity.is_none() {
        return Err(SignpostError::Empty);
    }
    if let Some(text) = &description {
        check_cap("description", text, MAX_DESCRIPTION_CHARS)?;
    }
    if let Some(line) = &activity {
        check_cap("activity", &line.text, MAX_ACTIVITY_CHARS)?;
        parse_day(&line.day)?;
    }
    Ok((description, activity))
}

/// Where a project's signposts go, and under whose name.
struct Target {
    owner_wiki_id: WikiId,
    owner_principal: Principal,
    source_path: String,
    title: String,
    allow: Vec<Principal>,
}

/// Resolve the project wiki, check the caller owns it, and locate the
/// owner's own standard wiki — the signpost is a fact about the owner's
/// world, so it lives where their facts live, not in the project.
fn resolve_target(tree: &WikiTree, caller: &str, project_wiki_id: &WikiId) -> Result<Target> {
    let project = tree.locate(project_wiki_id)?;
    if !project.meta().smart {
        return Err(SignpostError::NotSmart {
            wiki_id: project_wiki_id.as_str().to_owned(),
        });
    }
    let Principal::User(owner) = tree.resolve_scope_principal(project.meta())? else {
        return Err(SignpostError::GroupOwned {
            wiki_id: project_wiki_id.as_str().to_owned(),
        });
    };
    if owner != caller {
        return Err(SignpostError::NotOwner {
            wiki_id: project_wiki_id.as_str().to_owned(),
            owner,
            caller: caller.to_owned(),
        });
    }
    // The scope principal of a root identity wiki IS its wiki id, so the
    // owner's standard wiki is reachable by that name.
    let owner_wiki_id = owner_wiki_id(&owner)?;
    let owner_handle = tree.locate(&owner_wiki_id)?;
    let source_path = crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &owner_handle.abs_dir().join(crate::wiki::PROJECTS_FILENAME),
    );
    Ok(Target {
        owner_wiki_id,
        owner_principal: Principal::User(owner),
        source_path,
        title: project_title(project.meta()),
        // Read access mirrors the project's: whoever may read the project
        // may know it exists. The docs the signpost points at are gated
        // independently by the smart-wiki registry, so the two never
        // diverge.
        allow: project.meta().shared_with.clone(),
    })
}

/// Arguments of [`put`], grouped to keep the call sites readable.
struct PutRequest<'a> {
    wiki_id: &'a str,
    page: &'a PathBuf,
    body: String,
    owner: &'a Principal,
    allow: &'a [Principal],
    topics: Vec<String>,
    previous: Option<&'a FactIndexRow>,
}

/// Write one signpost, superseding its predecessor when there is one.
///
/// Dedup is forced off: identity here is the topic key, not similarity —
/// two projects described in similar words are two signposts, and a
/// refreshed description must replace its predecessor rather than fold
/// into it.
async fn put(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    req: PutRequest<'_>,
) -> Result<SignpostOutcome> {
    if let Some(prev) = req.previous
        && prev.text == req.body
    {
        return Ok(SignpostOutcome::Unchanged(prev.fact_id.clone()));
    }
    let capture_req = CaptureRequest {
        wiki_id: owner_wiki_id(req.wiki_id)?,
        page: req.page.clone(),
        body: req.body,
        owner: req.owner.clone(),
        allow: req.allow.to_vec(),
        sender: Some(req.owner.clone()),
        fact_type: Some(SIGNPOST_FACT_TYPE.to_owned()),
        topics: req.topics,
        // Off: see the doc comment above.
        dedup_threshold: Some(1.01),
        valid_from: None,
        valid_to: None,
        style: None,
        page_description: None,
        salience: None,
        authored_refs: Vec::new(),
    };
    if let Some(prev) = req.previous {
        let outcome =
            capture::wiki_supersede(tree, pool, embedder, &prev.fact_id, capture_req).await?;
        return Ok(SignpostOutcome::Updated(outcome.fact_id));
    }
    let outcome = capture::wiki_capture(tree, pool, embedder, capture_req).await?;
    match outcome.action {
        CaptureAction::Captured { .. } => Ok(SignpostOutcome::Created(outcome.fact_id)),
        // Unreachable with dedup off, but a skip must not read as a
        // successful write.
        _ => Ok(SignpostOutcome::Unchanged(outcome.fact_id)),
    }
}

/// Tombstone this project's activity lines that have fallen out of the
/// window, and count what remains. Returns `(retired, active_days)`.
async fn roll_window(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    source_path: &str,
    project_wiki_id: &str,
) -> Result<(usize, usize)> {
    let days: Vec<(FactId, NaiveDate)> = fact_index::find_active_by_source_path(pool, source_path)
        .await?
        .iter()
        .filter(|row| is_signpost_for(row, project_wiki_id))
        .filter_map(|row| {
            day_of(row)
                .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
                .map(|d| (row.fact_id.clone(), d))
        })
        .collect();
    // The window is measured from the freshest day ON RECORD, not from
    // the clock: a project dormant for a month keeps its last few lines
    // (they still say when they happened) instead of silently emptying
    // the moment someone writes to a sibling project.
    let Some(newest) = days.iter().map(|(_, d)| *d).max() else {
        return Ok((0, 0));
    };
    let cutoff = newest - chrono::Duration::days(ACTIVITY_WINDOW_DAYS - 1);
    let mut retired = 0;
    let mut kept = 0;
    for (fact_id, day) in &days {
        if *day < cutoff {
            capture::wiki_forget(
                tree,
                pool,
                Arc::clone(&embedder),
                fact_id,
                "signpost_window",
            )
            .await?;
            retired += 1;
        } else {
            kept += 1;
        }
    }
    Ok((retired, kept))
}

fn check_cap(field: &'static str, text: &str, cap: usize) -> Result<()> {
    let actual = text.chars().count();
    if actual > cap {
        return Err(SignpostError::TooLong { field, cap, actual });
    }
    Ok(())
}

fn parse_day(day: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| SignpostError::BadDay {
        got: day.to_owned(),
    })
}

/// Display name of a project: its title, or its slug when untitled.
fn project_title(meta: &crate::wiki::WikiMeta) -> String {
    let title = meta.title.trim();
    if title.is_empty() {
        meta.slug.as_str().to_owned()
    } else {
        title.to_owned()
    }
}

/// `"AcmeSigns — <text>"`, unless the text already opens with the name.
///
/// The name has to be *in* the fact: a signpost is read as a standalone
/// line in a recall block, where "un sistema per i cartelli digitali"
/// with no subject signposts nothing.
fn description_body(title: &str, text: &str) -> String {
    if text.to_lowercase().starts_with(&title.to_lowercase()) {
        text.to_owned()
    } else {
        format!("{title} — {text}")
    }
}

/// `"2026-07-26 — AcmeSigns: <text>"`. Same reasoning as
/// [`description_body`], plus the day, which is the whole point of a
/// chronology line.
fn activity_body(day: &str, title: &str, text: &str) -> String {
    format!("{day} — {title}: {text}")
}

pub(crate) fn description_topics(project_wiki_id: &str) -> Vec<String> {
    vec![
        TOPIC_SIGNPOST.to_owned(),
        format!("{TOPIC_WIKI_PREFIX}{project_wiki_id}"),
        TOPIC_DESCRIPTION.to_owned(),
    ]
}

fn activity_topics(project_wiki_id: &str, day: &str) -> Vec<String> {
    vec![
        TOPIC_SIGNPOST.to_owned(),
        format!("{TOPIC_WIKI_PREFIX}{project_wiki_id}"),
        format!("{TOPIC_DAY_PREFIX}{day}"),
    ]
}

fn has_topic(row: &FactIndexRow, topic: &str) -> bool {
    row.topics.iter().any(|t| t == topic)
}

/// Whether this row is a signpost for the given project.
fn is_signpost_for(row: &FactIndexRow, project_wiki_id: &str) -> bool {
    has_topic(row, TOPIC_SIGNPOST)
        && has_topic(row, &format!("{TOPIC_WIKI_PREFIX}{project_wiki_id}"))
}

/// The day an activity-line row carries, if it is one.
fn day_of(row: &FactIndexRow) -> Option<String> {
    row.topics
        .iter()
        .find_map(|t| t.strip_prefix(TOPIC_DAY_PREFIX))
        .map(str::to_owned)
}

/// The project wiki id a signpost row points at, if it is a signpost.
///
/// The reader half of the channel: recall uses it to open the project a
/// surfaced signpost names ([`crate::recall::recall_project_docs`]).
#[must_use]
pub fn project_of(row: &FactIndexRow) -> Option<String> {
    if !has_topic(row, TOPIC_SIGNPOST) {
        return None;
    }
    row.topics
        .iter()
        .find_map(|t| t.strip_prefix(TOPIC_WIKI_PREFIX))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

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
        Arc::new(FakeEmbedder::new("fake-bge-m3", 8))
    }

    /// `alice` (identity root) + `alice-acmesigns` (smart child).
    fn seed_tree(dir: &std::path::Path, shared_with: &str) -> WikiTree {
        let alice = dir.join("wikis/alice");
        std::fs::create_dir_all(&alice).unwrap();
        std::fs::write(
            alice.join("_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\nslug: alice\ntitle: Alice\n---\n",
        )
        .unwrap();
        let project = dir.join("wikis/alice/acmesigns");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("_meta.md"),
            format!(
                "---\nwiki_id: alice-acmesigns\nwiki_type: wiki-tech\nparent_wiki_id: alice\nslug: acmesigns\ntitle: AcmeSigns\nsmart: true\n{shared_with}---\n"
            ),
        )
        .unwrap();
        WikiTree::open(dir).expect("open tree")
    }

    fn req(description: Option<&str>, day: Option<(&str, &str)>) -> SignpostRequest {
        SignpostRequest {
            project_wiki_id: WikiId::parse("alice-acmesigns").unwrap(),
            description: description.map(str::to_owned),
            activity: day.map(|(d, t)| ActivityLine {
                day: d.to_owned(),
                text: t.to_owned(),
            }),
        }
    }

    async fn page_facts(pool: &SqlitePool) -> Vec<FactIndexRow> {
        fact_index::find_active_by_source_path(pool, "wikis/alice/projects.md")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn status_stays_silent_on_the_consumers_own_operational_wiki() {
        let dir = tempdir().unwrap();
        seed_tree(dir.path(), "");
        // The agent's own operational wiki, exactly as the sign-in flow
        // forges it: smart, owned by the same user, `wiki_type: agent`.
        let agent = dir.path().join("wikis/alice/cc-laptop");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("_meta.md"),
            "---\nwiki_id: alice-cc-laptop\nwiki_type: agent\nparent_wiki_id: alice\nslug: cc-laptop\ntitle: Claude Code (cc-laptop)\nsmart: true\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).expect("reopen tree");
        let pool = make_pool().await;

        // A project wiki reports (and, having no description yet, is what
        // makes `wiki_admin_push` volunteer the nudge).
        let project = status(&pool, &tree, &WikiId::parse("alice-acmesigns").unwrap())
            .await
            .expect("status")
            .expect("a project wiki has signpost state");
        assert!(!project.has_description);

        // Private working memory is not a project: no state, so no nudge.
        assert!(
            status(&pool, &tree, &WikiId::parse("alice-cc-laptop").unwrap())
                .await
                .expect("status")
                .is_none()
        );
    }

    /// The same silence, keyed on the marker the SERVER writes rather than on
    /// the `wiki_type` string the consumer chooses. A consumer that pushed its
    /// operational wiki under some other label — anything is legal there — used
    /// to collect signpost nudges on its private working memory.
    #[tokio::test]
    async fn status_stays_silent_on_an_operational_wiki_under_any_label() {
        let dir = tempdir().unwrap();
        seed_tree(dir.path(), "");
        let agent = dir.path().join("wikis/alice/cc-laptop");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("_meta.md"),
            "---\nwiki_id: alice-cc-laptop\nwiki_type: wiki-scratch\nparent_wiki_id: alice\n\
             slug: cc-laptop\ntitle: Claude Code (cc-laptop)\nsmart: true\nis_agent: true\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).expect("reopen tree");
        let pool = make_pool().await;

        assert!(
            status(&pool, &tree, &WikiId::parse("alice-cc-laptop").unwrap())
                .await
                .expect("status")
                .is_none(),
            "the is_agent marker settles it, whatever the label says"
        );
    }

    #[tokio::test]
    async fn a_signpost_lands_on_the_owners_reserved_page_carrying_the_project_name() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let report = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(
                Some("sistema per gestire i cartelli digitali nei negozi"),
                None,
            ),
        )
        .await
        .expect("write");

        assert_eq!(report.owner_wiki_id, "alice");
        assert_eq!(report.source_path, "wikis/alice/projects.md");
        assert!(matches!(
            report.description,
            Some(SignpostOutcome::Created(_))
        ));

        let facts = page_facts(&pool).await;
        assert_eq!(facts.len(), 1);
        // The name has to be inside the fact: a recall block quotes the
        // line alone.
        assert!(
            facts[0].text.starts_with("AcmeSigns — "),
            "{}",
            facts[0].text
        );
        assert_eq!(facts[0].fact_type.as_deref(), Some(SIGNPOST_FACT_TYPE));
        assert_eq!(
            project_of(&facts[0]).as_deref(),
            Some("alice-acmesigns"),
            "the reader half must be able to name the project"
        );
        // And it is on disk, in the page the recall navigator reads.
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/projects.md")).unwrap();
        assert!(page.contains("AcmeSigns — sistema per gestire"));
    }

    #[tokio::test]
    async fn refreshing_an_unchanged_description_writes_nothing() {
        // The skill asks the smart consumer to refresh on every push, so
        // the no-op path is the common one — it must not churn the page,
        // the embedding, or the fact id.
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let first = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(Some("un sistema"), None),
        )
        .await
        .unwrap();
        let again = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(Some("un sistema"), None),
        )
        .await
        .unwrap();

        assert!(matches!(
            again.description,
            Some(SignpostOutcome::Unchanged(_))
        ));
        assert_eq!(
            again.description.unwrap().fact_id(),
            first.description.unwrap().fact_id()
        );
        assert_eq!(page_facts(&pool).await.len(), 1);
    }

    #[tokio::test]
    async fn a_new_description_supersedes_the_previous_one() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(Some("prima versione"), None),
        )
        .await
        .unwrap();
        let second = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(Some("seconda versione"), None),
        )
        .await
        .unwrap();

        assert!(matches!(
            second.description,
            Some(SignpostOutcome::Updated(_))
        ));
        let facts = page_facts(&pool).await;
        assert_eq!(facts.len(), 1, "one active description per project");
        assert!(facts[0].text.contains("seconda versione"));
    }

    #[tokio::test]
    async fn activity_lines_roll_out_of_the_window() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        for (day, text) in [
            ("2026-07-20", "ha iniziato il modulo di consegna"),
            ("2026-07-21", "ha corretto un errore sui contenuti"),
            ("2026-07-26", "ha rifatto la pagina di stato"),
        ] {
            write(
                &pool,
                &tree,
                embedder(),
                "alice",
                req(None, Some((day, text))),
            )
            .await
            .unwrap();
        }

        // Window is measured from the freshest day on record: 07-26 keeps
        // back to 07-22, so both older lines are gone.
        let days: Vec<String> = page_facts(&pool).await.iter().filter_map(day_of).collect();
        assert_eq!(days, vec!["2026-07-26".to_owned()]);
        assert_eq!(
            last_activity_day(&pool, "wikis/alice/projects.md", "alice-acmesigns")
                .await
                .unwrap()
                .as_deref(),
            Some("2026-07-26")
        );
    }

    #[tokio::test]
    async fn the_same_day_written_twice_supersedes_instead_of_stacking() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(None, Some(("2026-07-26", "ha corretto un errore"))),
        )
        .await
        .unwrap();
        let second = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(
                None,
                Some((
                    "2026-07-26",
                    "ha corretto un errore e rifatto la pagina di stato",
                )),
            ),
        )
        .await
        .unwrap();

        assert!(matches!(second.activity, Some(SignpostOutcome::Updated(_))));
        let facts = page_facts(&pool).await;
        assert_eq!(facts.len(), 1);
        assert!(facts[0].text.starts_with("2026-07-26 — AcmeSigns: "));
        assert!(facts[0].text.contains("pagina di stato"));
    }

    #[tokio::test]
    async fn the_caps_are_enforced_by_the_server_not_asked_of_the_agent() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let long = "a".repeat(MAX_DESCRIPTION_CHARS + 1);
        let err = write(&pool, &tree, embedder(), "alice", req(Some(&long), None))
            .await
            .expect_err("over the cap");
        assert!(
            matches!(
                err,
                SignpostError::TooLong {
                    field: "description",
                    ..
                }
            ),
            "{err:?}"
        );

        let long_day = "b".repeat(MAX_ACTIVITY_CHARS + 1);
        let err = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(None, Some(("2026-07-26", &long_day))),
        )
        .await
        .expect_err("over the cap");
        assert!(
            matches!(
                err,
                SignpostError::TooLong {
                    field: "activity",
                    ..
                }
            ),
            "{err:?}"
        );
        // Refused, never truncated: nothing was written.
        assert!(page_facts(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn only_the_owner_of_the_project_may_signpost_it() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let err = write(
            &pool,
            &tree,
            embedder(),
            "bob",
            req(Some("un sistema"), None),
        )
        .await
        .expect_err("foreign caller");
        assert!(matches!(err, SignpostError::NotOwner { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_standard_wiki_cannot_be_signposted() {
        // Signposts point AT project documentation. A standard wiki's
        // facts are recalled directly — a pointer would be noise.
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let err = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            SignpostRequest {
                project_wiki_id: WikiId::parse("alice").unwrap(),
                description: Some("me stessa".to_owned()),
                activity: None,
            },
        )
        .await
        .expect_err("standard wiki");
        assert!(matches!(err, SignpostError::NotSmart { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn read_access_mirrors_the_projects_sharing_roster() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "shared_with: [user:bob]\n");
        let pool = make_pool().await;

        write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(Some("un sistema"), None),
        )
        .await
        .unwrap();

        let facts = page_facts(&pool).await;
        assert_eq!(facts[0].owner_id, Principal::User("alice".to_owned()));
        assert_eq!(
            facts[0].allow_ids,
            vec![Principal::User("bob".to_owned())],
            "whoever may read the project may know it exists"
        );
    }

    #[tokio::test]
    async fn a_malformed_day_is_refused() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        let err = write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(None, Some(("26/07/2026", "qualcosa"))),
        )
        .await
        .expect_err("bad day");
        assert!(matches!(err, SignpostError::BadDay { .. }), "{err:?}");
    }

    #[test]
    fn a_description_that_opens_with_the_name_is_not_prefixed_twice() {
        assert_eq!(
            description_body("AcmeSigns", "AcmeSigns è un sistema"),
            "AcmeSigns è un sistema"
        );
        assert_eq!(
            description_body("AcmeSigns", "un sistema"),
            "AcmeSigns — un sistema"
        );
    }
}
