// SPDX-License-Identifier: AGPL-3.0-or-later
//! Project **signposts** — the standard agent learns that a project exists.
//!
//! A conversational turn recalls facts, never smart-wiki sections: that is
//! what keeps a personal exchange from being buried under project
//! documentation ([`crate::sections`]). The price is that a project the
//! user never *names* is invisible to their standard agent — the memory
//! cannot connect a dot it cannot see.
//!
//! A signpost is the dot. It is a short fact in the owner's own standard
//! wiki, and there are two kinds, on **two reserved pages**:
//!
//! - **one description per project**, on [`crate::wiki::PROJECTS_FILENAME`]
//!   — what it is and what it is for, in plain language,
//!   [`MAX_DESCRIPTION_CHARS`] at most;
//! - **one activity line per project per day**, on
//!   [`crate::wiki::PROJECT_DIARY_FILENAME`] — what happened that day,
//!   [`MAX_ACTIVITY_CHARS`] at most, kept for [`ACTIVITY_WINDOW_DAYS`].
//!
//! Two pages because the halves have **opposite lifecycles** — one is
//! derived and rebuildable, the other accumulated and irreplaceable — and
//! sharing a page would cost the stronger guarantee: `projects.md` is
//! regenerable from the registry in full, and only stays that way while
//! nothing on it had to be kept. The table in
//! [`crate::wiki::PROJECT_DIARY_FILENAME`] is the whole argument.
//!
//! For **delivery** they are equivalent: a fact surfacing from either page
//! says *this project is in play*, which is what offers to open its
//! documentation ([`crate::wiki::is_signpost_page`]). A diary line is at
//! least as good a signal as a description — «what did I do on X?»
//! surfaces the diary, and the details it wants are in the project wiki.
//!
//! ## The description is a projection; only the activity line is written
//!
//! The two halves reach this page by different routes, and the difference
//! is the whole point.
//!
//! A **description** is a *property* of the project — it changes when the
//! project changes, not when something happens. So it is authored once in
//! the project's own `_meta.md`, mirrored into `smart_wikis.description`,
//! and written here by [`project_descriptions`] on every sweep. Nobody
//! calls anything. It used to be written by the smart consumer through
//! [`write`], and the counting is why that stopped: across the whole
//! recorded window, four projects on this deployment ever had a row
//! written, and the largest undescribed corpus was 1 477 sections with
//! none. Nothing was lost to concurrency — **nobody ever wrote anything**.
//! A nudge fired on every push and was ignored, because a nudge is advice,
//! and an act nobody performs leaves no trace to notice. A property does:
//! an undescribed project is an empty column.
//!
//! An **activity line** is an *event*, so it stays a write — there is
//! nothing to derive it from. [`write`] remains its path.
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
///
/// Public because the smart-corpus funnel
/// ([`crate::recall::admitted_smart_wikis`]) selects on it: the description
/// is what decides whether a project's sections may be read at all.
pub const TOPIC_DESCRIPTION: &str = "signpost-description";

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

    /// The smart-wiki registry could not be read — the projection sweep's
    /// only hard dependency, since it is what says which projects exist.
    #[error("signpost registry: {0}")]
    Registry(#[from] crate::sections::SectionError),

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
    let diary_page = PathBuf::from(crate::wiki::PROJECT_DIARY_FILENAME);

    // The two halves live on two pages, because they have opposite
    // lifecycles: the door signs are derived and rebuildable, the diary is
    // accumulated and windowed (see `wiki::PROJECT_DIARY_FILENAME`). Each
    // page is small — a handful of facts — so one scan apiece serves every
    // lookup below.
    let for_this_project = |rows: Vec<FactIndexRow>| -> Vec<FactIndexRow> {
        rows.into_iter()
            .filter(|row| is_signpost_for(row, req.project_wiki_id.as_str()))
            .collect()
    };
    let existing =
        for_this_project(fact_index::find_active_by_source_path(pool, &target.source_path).await?);
    let existing_diary = for_this_project(
        fact_index::find_active_by_source_path(pool, &target.diary_source_path).await?,
    );

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
        let previous = existing_diary.iter().find(|row| has_topic(row, &day_topic));
        report.activity = Some(
            put(
                pool,
                tree,
                Arc::clone(&embedder),
                PutRequest {
                    wiki_id: target.owner_wiki_id.as_str(),
                    page: &diary_page,
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
        &target.diary_source_path,
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

/// What one sweep of [`project_descriptions`] changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectionReport {
    /// Description facts newly written.
    pub created: usize,
    /// Description facts whose text moved and were superseded.
    pub updated: usize,
    /// Projects whose description was already identical — no write.
    pub unchanged: usize,
    /// Description facts retired because the project no longer declares
    /// one (the line was removed from `_meta.md`).
    pub retired: usize,
    /// Registry rows that could not be projected — the wiki is gone from
    /// the tree, or is group-owned. Logged, never fatal.
    pub skipped: usize,
}

/// Project every project's `smart_wikis.description` into a signpost fact
/// on its owner's `projects.md`, so ordinary flat recall can find it.
///
/// **Why a projection and not the thing itself.** A standard consumer's
/// per-turn recall reads the fact corpus only, so a description that lives
/// solely in a registry column is invisible to it: the door has to *be a
/// fact*, found by the same ranking as every other door, or it needs a
/// special case in the prompt that grows with the project count. So the
/// description is authored once as a property (the wiki's own `_meta`) and
/// mirrored here as data. Two places, one direction, and they cannot
/// diverge because only one of them is ever written by hand.
///
/// This **replaces** the description half of [`write`] as the way the fact
/// comes to exist. That path required a smart agent to remember a separate
/// tool call, and the counting says it did not: across the whole recorded
/// window, four projects on this deployment ever had a row written and the
/// largest undescribed corpus was 1 477 sections with none. A nudge fired
/// on every push and was ignored, because a nudge is advice.
///
/// Idempotent, and cheap when nothing moved: one scan of each owner's
/// signpost page, then a write only where the text actually differs.
/// Removing the line from `_meta.md` retires the fact — otherwise a door
/// would stay open onto a project that had stopped describing itself.
///
/// # Errors
///
/// Storage failures. A single unprojectable wiki is counted in
/// [`ProjectionReport::skipped`] and does not fail the sweep.
pub async fn project_descriptions(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
) -> Result<ProjectionReport> {
    let mut report = ProjectionReport::default();
    let page = PathBuf::from(crate::wiki::PROJECTS_FILENAME);

    for row in crate::sections::list_smart_wikis(pool).await? {
        let Ok(project_wiki_id) = WikiId::parse(&row.wiki_id) else {
            report.skipped += 1;
            continue;
        };
        let target = match target_owner(tree, &project_wiki_id)
            .and_then(|owner| target_for(tree, &project_wiki_id, owner))
        {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(
                    wiki_id = %row.wiki_id,
                    error = %e,
                    "signpost projection: wiki not projectable, skipped"
                );
                report.skipped += 1;
                continue;
            },
        };

        let existing: Vec<FactIndexRow> =
            fact_index::find_active_by_source_path(pool, &target.source_path)
                .await?
                .into_iter()
                .filter(|r| is_signpost_for(r, &row.wiki_id))
                .collect();
        let previous = existing.iter().find(|r| has_topic(r, TOPIC_DESCRIPTION));

        let Some(text) = row.description.as_deref() else {
            // The wiki stopped declaring a description: close the door
            // rather than leave it open onto nothing.
            if let Some(prev) = previous {
                capture::wiki_forget(
                    tree,
                    pool,
                    Arc::clone(&embedder),
                    &prev.fact_id,
                    "signpost_description_withdrawn",
                )
                .await?;
                report.retired += 1;
            }
            continue;
        };

        let body = description_body(&target.title, text);
        match put(
            pool,
            tree,
            Arc::clone(&embedder),
            PutRequest {
                wiki_id: target.owner_wiki_id.as_str(),
                page: &page,
                body,
                owner: &target.owner_principal,
                allow: &target.allow,
                topics: description_topics(&row.wiki_id),
                previous,
            },
        )
        .await?
        {
            SignpostOutcome::Created(_) => report.created += 1,
            SignpostOutcome::Updated(_) => report.updated += 1,
            SignpostOutcome::Unchanged(_) => report.unchanged += 1,
        }
    }

    if report.created + report.updated + report.retired > 0 {
        tracing::info!(
            created = report.created,
            updated = report.updated,
            retired = report.retired,
            unchanged = report.unchanged,
            skipped = report.skipped,
            "signpost projection: descriptions synced from the registry"
        );
    }
    Ok(report)
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
    let (Ok(page), Ok(diary)) = (page_path(tree, &owner), diary_page_path(tree, &owner)) else {
        return Ok(None);
    };
    // Two pages now, one question each: the door sign is on `projects.md`
    // and the days are in the diary.
    let mine = |rows: Vec<FactIndexRow>| -> Vec<FactIndexRow> {
        rows.into_iter()
            .filter(|row| is_signpost_for(row, project_wiki_id.as_str()))
            .collect()
    };
    let signs = mine(fact_index::find_active_by_source_path(pool, &page).await?);
    let days = mine(fact_index::find_active_by_source_path(pool, &diary).await?);
    Ok(Some(SignpostStatus {
        has_description: signs.iter().any(|row| has_topic(row, TOPIC_DESCRIPTION)),
        last_activity_day: days.iter().filter_map(day_of).max(),
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

/// Workdir-relative path of `owner_user`'s project **diary**.
///
/// # Errors
///
/// The user id is not a usable wiki id, or has no wiki on disk.
pub fn diary_page_path(tree: &WikiTree, owner_user: &str) -> Result<String> {
    let handle = tree.locate(&owner_wiki_id(owner_user)?)?;
    Ok(crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &handle.abs_dir().join(crate::wiki::PROJECT_DIARY_FILENAME),
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
    /// Where the project's door sign lives — `projects.md`, derived.
    source_path: String,
    /// Where its diary lines live — `project_diary.md`, accumulated.
    diary_source_path: String,
    title: String,
    allow: Vec<Principal>,
}

/// Resolve the project wiki, check the caller owns it, and locate the
/// owner's own standard wiki — the signpost is a fact about the owner's
/// world, so it lives where their facts live, not in the project.
fn resolve_target(tree: &WikiTree, caller: &str, project_wiki_id: &WikiId) -> Result<Target> {
    let owner = target_owner(tree, project_wiki_id)?;
    if owner != caller {
        return Err(SignpostError::NotOwner {
            wiki_id: project_wiki_id.as_str().to_owned(),
            owner,
            caller: caller.to_owned(),
        });
    }
    target_for(tree, project_wiki_id, owner)
}

/// The user a project's signposts belong to — the same check
/// [`resolve_target`] makes, minus the caller comparison.
fn target_owner(tree: &WikiTree, project_wiki_id: &WikiId) -> Result<String> {
    let project = tree.locate(project_wiki_id)?;
    if !project.meta().smart {
        return Err(SignpostError::NotSmart {
            wiki_id: project_wiki_id.as_str().to_owned(),
        });
    }
    match tree.resolve_scope_principal(project.meta())? {
        Principal::User(owner) => Ok(owner),
        Principal::Group(_) => Err(SignpostError::GroupOwned {
            wiki_id: project_wiki_id.as_str().to_owned(),
        }),
    }
}

/// Where a project's signposts live, and under whose name. Split out of
/// [`resolve_target`] so the **server** can write a projection for a wiki
/// nobody is currently calling on behalf of — the ownership check is a
/// property of a *caller*, and there is no caller here.
fn target_for(tree: &WikiTree, project_wiki_id: &WikiId, owner: String) -> Result<Target> {
    let project = tree.locate(project_wiki_id)?;
    // The scope principal of a root identity wiki IS its wiki id, so the
    // owner's standard wiki is reachable by that name.
    let owner_wiki_id = owner_wiki_id(&owner)?;
    let owner_handle = tree.locate(&owner_wiki_id)?;
    let source_path = crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &owner_handle.abs_dir().join(crate::wiki::PROJECTS_FILENAME),
    );
    let diary_source_path = crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &owner_handle
            .abs_dir()
            .join(crate::wiki::PROJECT_DIARY_FILENAME),
    );
    Ok(Target {
        owner_wiki_id,
        owner_principal: Principal::User(owner),
        source_path,
        diary_source_path,
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

    /// The door-sign page — derived, rebuildable.
    async fn page_facts(pool: &SqlitePool) -> Vec<FactIndexRow> {
        fact_index::find_active_by_source_path(pool, "wikis/alice/projects.md")
            .await
            .unwrap()
    }

    /// The diary page — accumulated, windowed. A separate page precisely so
    /// the two cannot damage each other.
    async fn diary_facts(pool: &SqlitePool) -> Vec<FactIndexRow> {
        fact_index::find_active_by_source_path(pool, "wikis/alice/project_diary.md")
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
        let days: Vec<String> = diary_facts(&pool).await.iter().filter_map(day_of).collect();
        assert_eq!(days, vec!["2026-07-26".to_owned()]);
        assert!(
            page_facts(&pool).await.is_empty(),
            "an activity line never lands on the door-sign page"
        );
        assert_eq!(
            last_activity_day(&pool, "wikis/alice/project_diary.md", "alice-acmesigns")
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
        let facts = diary_facts(&pool).await;
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
    /// Seed the registry the way `project_smart_wiki_registry` would, so the
    /// projection has something to mirror without dragging in a tree sweep.
    async fn register(pool: &SqlitePool, wiki_id: &str, description: Option<&str>) {
        crate::sections::upsert_smart_wiki(
            pool,
            &crate::sections::SmartWikiRow {
                wiki_id: wiki_id.to_owned(),
                slug: "acmesigns".to_owned(),
                owner_id: Principal::User("alice".to_owned()),
                shared_with: Vec::new(),
                project_id: None,
                wiki_type: "project".to_owned(),
                description: description.map(str::to_owned),
            },
        )
        .await
        .expect("register");
    }

    async fn description_of(pool: &SqlitePool) -> Option<String> {
        fact_index::find_active_by_source_path(pool, "wikis/alice/projects.md")
            .await
            .expect("scan")
            .into_iter()
            .find(|r| has_topic(r, TOPIC_DESCRIPTION))
            .map(|r| r.text)
    }

    /// The whole point: a description authored as a PROPERTY becomes a fact
    /// the ordinary flat recall can rank, without anyone calling a tool.
    #[tokio::test]
    async fn projection_writes_the_registry_description_as_a_fact() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;
        register(&pool, "alice-acmesigns", Some("Signage for shop windows.")).await;

        let r = project_descriptions(&pool, &tree, embedder())
            .await
            .expect("project");
        assert_eq!((r.created, r.updated, r.retired), (1, 0, 0));
        let body = description_of(&pool).await.expect("a description fact");
        assert!(body.contains("Signage for shop windows."));
        assert!(
            body.contains("AcmeSigns"),
            "the project's title leads the line"
        );
    }

    /// Re-running must not churn the page, the embeddings or the recall
    /// counters — the sweep runs on every safety-net tick.
    #[tokio::test]
    async fn projection_is_a_no_op_when_nothing_moved() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;
        register(&pool, "alice-acmesigns", Some("Signage for shop windows.")).await;

        project_descriptions(&pool, &tree, embedder())
            .await
            .unwrap();
        let second = project_descriptions(&pool, &tree, embedder())
            .await
            .expect("project");
        assert_eq!(
            (second.created, second.updated, second.unchanged),
            (0, 0, 1)
        );
    }

    /// Editing `_meta.md` moves the door sign, and the old one does not
    /// survive alongside it.
    #[tokio::test]
    async fn projection_supersedes_a_changed_description() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;
        register(&pool, "alice-acmesigns", Some("Signage for shop windows.")).await;
        project_descriptions(&pool, &tree, embedder())
            .await
            .unwrap();

        register(
            &pool,
            "alice-acmesigns",
            Some("Queue displays for pharmacies."),
        )
        .await;
        let r = project_descriptions(&pool, &tree, embedder())
            .await
            .expect("project");
        assert_eq!((r.created, r.updated), (0, 1));

        let live: Vec<String> =
            fact_index::find_active_by_source_path(&pool, "wikis/alice/projects.md")
                .await
                .unwrap()
                .into_iter()
                .filter(|r| has_topic(r, TOPIC_DESCRIPTION))
                .map(|r| r.text)
                .collect();
        assert_eq!(live.len(), 1, "exactly one description stays active");
        assert!(live[0].contains("Queue displays"));
    }

    /// Withdrawing the line closes the door. Leaving the fact behind would
    /// keep pointing at a project that had stopped describing itself.
    #[tokio::test]
    async fn projection_retires_a_withdrawn_description() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;
        register(&pool, "alice-acmesigns", Some("Signage for shop windows.")).await;
        project_descriptions(&pool, &tree, embedder())
            .await
            .unwrap();
        assert!(description_of(&pool).await.is_some());

        register(&pool, "alice-acmesigns", None).await;
        let r = project_descriptions(&pool, &tree, embedder())
            .await
            .expect("project");
        assert_eq!(r.retired, 1);
        assert!(description_of(&pool).await.is_none());
    }

    /// A registry row whose wiki has left the tree must not fail the sweep
    /// for everyone else.
    #[tokio::test]
    async fn projection_skips_a_wiki_that_is_gone_and_keeps_going() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;
        register(&pool, "alice-ghost", Some("A wiki that is not on disk.")).await;
        register(&pool, "alice-acmesigns", Some("Signage for shop windows.")).await;

        let r = project_descriptions(&pool, &tree, embedder())
            .await
            .expect("the sweep survives");
        assert_eq!(r.skipped, 1);
        assert_eq!(r.created, 1, "the healthy project still got its door");
    }
    /// The separation, stated once: a project's door sign and its diary go
    /// to two different pages, so the derived one stays fully rebuildable
    /// and the accumulated one cannot be clobbered by rebuilding it.
    #[tokio::test]
    async fn the_door_sign_and_the_diary_land_on_two_pages() {
        let dir = tempdir().unwrap();
        let tree = seed_tree(dir.path(), "");
        let pool = make_pool().await;

        write(
            &pool,
            &tree,
            embedder(),
            "alice",
            req(
                Some("Signage for shop windows."),
                Some(("2026-07-26", "ha rifatto la pagina di stato")),
            ),
        )
        .await
        .unwrap();

        let signs = page_facts(&pool).await;
        assert_eq!(signs.len(), 1, "the door sign, alone on projects.md");
        assert!(has_topic(&signs[0], TOPIC_DESCRIPTION));

        let diary = diary_facts(&pool).await;
        assert_eq!(diary.len(), 1, "the day, alone on the diary page");
        assert_eq!(day_of(&diary[0]).as_deref(), Some("2026-07-26"));

        // Both pages are signpost pages for delivery: a diary line
        // surfacing means the project is in play just as a description does.
        assert!(crate::wiki::is_signpost_page("wikis/alice/projects.md"));
        assert!(crate::wiki::is_signpost_page(
            "wikis/alice/project_diary.md"
        ));
        // …and both stay fenced out of the structural sweeps.
        assert!(crate::wiki::is_channel_page("wikis/alice/project_diary.md"));
        assert!(!crate::wiki::is_signpost_page(
            "wikis/alice/my_projects_notes.md"
        ));
    }
}
