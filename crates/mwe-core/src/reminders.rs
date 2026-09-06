// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dated commitments that have come **due** — the memory noticing that
//! something it already holds has arrived, and telling the person.
//!
//! ## What this is not
//!
//! Not a scheduler. A consumer that lets its user say *"wake me at seven"*
//! or *"post to the group every Thursday"* owns that, and mwe-mcp has no
//! business in it: those are instructions to an assistant, not memory.
//! This module fires for exactly one thing — a **fact this memory already
//! stores, carrying a date, that has now come round**. The distinction is
//! the founder's (2026-07-30) and it is what keeps the engine out of the
//! alarm-clock business.
//!
//! The reason that narrow case cannot live in the consumer's scheduler
//! either: **only the memory learns that the appointment moved.** A job
//! written when the user first asked freezes both the time and the wording;
//! when a later turn says *"it slipped to Friday"* the memory records the
//! correction ([`crate::ingest`]'s `validity_edits`) and the frozen job
//! still fires on Thursday, with the old text. Firing from the fact means
//! the correction is automatically the thing that rings.
//!
//! ## What fires, and when
//!
//! `fact_type = "plan"` **with a concrete `valid_to` and no
//! `decay_reason`**. The type is already precise, and not by luck: the
//! ingest prompt forbids the one `plan` that must never ring — *"a shopping
//! item is NOT a TTL, it is closed later by a completing message, not by a
//! timer"* — so a consumable intention is stored with `valid_to: null` by
//! construction. A `state` ("in Berlin this week") expires rather than falls
//! due, and a `rule` never rings.
//!
//! **A `valid_to` has two authors and only one of them set a deadline.** The
//! classifier writes it at capture, from a date the speaker stated, and
//! leaves `decay_reason` NULL: that is a commitment, and it rings. Every
//! closure path writes it too — [`crate::fact_index::close_validity`] and
//! [`crate::fact_index::mark_superseded`] stamp `valid_to` together with a
//! `decay_reason` (completed, retracted, contradicted) — and there the value
//! is the instant the fact *stopped* holding, usually the instant of the
//! message that replaced it. Firing on that would ring a commitment minutes
//! after it was dropped, and did: four plans refined in conversation on
//! 2026-09-06 ("a bar of soap" → a chosen brand) reached a whole household
//! twelve minutes after being closed. So a stamped `decay_reason` is the
//! trace of a closure, and a closure never rings. A pure date correction
//! ([`crate::fact_index::set_validity`]) leaves the stamp alone precisely
//! because moving an appointment is not closing it.
//!
//! The firing instant is **derived**, not stored (decided on
//! the data): of the future-dated facts on the first production
//! deployment, **87 % carried a `valid_to` on a day boundary** —
//! `00:00:00` or `23:59:59`, a date with no hour in it. Firing *at*
//! `valid_to` would have rung at midnight for almost all of them, and a
//! separate `remind_at` column would have been **empty** for exactly those,
//! since nobody stated an hour to put in it. So:
//!
//! - a `valid_to` on a day boundary is a **date** → fire that date at
//!   [`ReminderPolicy::day_hour_utc`];
//! - any other clock time is an **instant somebody stated** → fire at
//!   `valid_to` minus [`ReminderPolicy::lead`].
//!
//! The hour is UTC, not local: the engine does no timezone arithmetic
//! anywhere (it hands the IANA zone to the classifier and lets the model
//! resolve wall-clock times), and giving this one module a timezone
//! database to itself would be the wrong place to start. For a deployment
//! whose people share a zone — a household, a team — one configured hour is
//! exactly right. Per-user local resolution is not implemented.
//!
//! ## Why a grace window, not a backlog
//!
//! The sweep only fires for an instant inside `(now - grace, now]`. Without
//! that bound the first run after this shipped would have emitted a notice
//! for **every** past dated commitment in the corpus — hundreds of pings
//! for appointments long gone. A missed window (the server was down) loses
//! that reminder rather than resurrecting it late, which is the right
//! trade: a reminder that arrives a day after the appointment is worse than
//! none.
//!
//! Idempotence is the existing `(kind, fact_id)` probe
//! ([`crate::events::find_recent_event_for`]) over a year-long window, so a
//! fact rings once even if the grace window covers several ticks.

use chrono::{DateTime, Datelike, Timelike, Utc};
use sqlx::SqlitePool;

use crate::events::{self, EventKind};
use crate::fact_index;

/// The `plan` classification — the only `fact_type` that rings.
const PLAN: &str = "plan";

/// How far back the candidate query reaches around `now`. Generous on
/// purpose: the firing instant is derived from `valid_to` and can sit up to
/// a day either side of it, so the SQL window is wide and the exact test
/// happens in [`firing_instant`].
const CANDIDATE_WINDOW_HOURS: i64 = 48;

/// Idempotence horizon for the `(kind, fact_id)` probe. A dated commitment
/// rings once, full stop; a year is "once" with room to spare.
///
/// Read here and by [`crate::housekeeping`], which keeps `reminder_due`
/// rows this long against the shorter retention it applies to the rest of
/// the queue: the probe can only answer while the row is still there.
pub(crate) const ALREADY_RUNG_DAYS: i64 = 365;

/// Operator knobs for the due sweep — the runtime half of the
/// `reminders:` config section.
#[derive(Debug, Clone, Copy)]
pub struct ReminderPolicy {
    /// Master switch. Off → the sweep is a no-op (it still runs, so the
    /// knob is hot-swappable without a restart).
    pub enabled: bool,
    /// UTC hour (0–23) at which a **date** with no stated time fires.
    pub day_hour_utc: u32,
    /// How long **before** a stated instant to fire. Zero = at the time.
    pub lead: chrono::Duration,
    /// How late a firing instant may be and still ring. Bounds the
    /// backlog after downtime.
    pub grace: chrono::Duration,
    /// Most notices one sweep may emit — a runaway guard, not a policy.
    pub cap: usize,
}

impl Default for ReminderPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            day_hour_utc: 7,
            lead: chrono::Duration::zero(),
            grace: chrono::Duration::hours(6),
            cap: 50,
        }
    }
}

/// One commitment that has come due, ready to be announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueReminder {
    /// The fact that came due.
    pub fact_id: String,
    /// Its home wiki — also the `dashboard_path` anchor.
    pub wiki_id: String,
    /// Everyone the commitment rings for, each `user:`-prefixed: its subject
    /// and the people it was shared with.
    ///
    /// A commitment reaches whoever it concerns, and who it concerns is the
    /// same question the retraction gate answers — the fact's own audience,
    /// not an inference from its subject (founder, 2026-09-06). Reading the
    /// subject alone is not a near-enough approximation of that: measured on
    /// the live memory that day, of 135 due commitments it would leave 16
    /// silent (the ones a group owns, and a group has no single inbox) and
    /// put 112 more in front of one member of the household they had been
    /// told to.
    pub recipients: Vec<String>,
    /// The fact's own prose. The notice carries it so the delivering
    /// agent can say the thing without a recall round-trip — the same
    /// reason `fact_minted_for_you` carries bodies.
    pub body: String,
    /// The stored `valid_to` this was derived from.
    pub due_at: String,
    /// The instant the policy resolved it to.
    pub fires_at: DateTime<Utc>,
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Facts examined inside the candidate window.
    pub examined: usize,
    /// Notices emitted.
    pub emitted: Vec<String>,
    /// Due facts skipped because a notice already exists for them.
    pub already_rung: usize,
}

/// Resolve a stored `valid_to` into the instant a reminder should fire.
///
/// A day boundary (`00:00:00` / `23:59:59`) means *a date, no hour stated*
/// and fires at `day_hour_utc` on that date; anything else is an instant
/// somebody said and fires `lead` before it. Nobody schedules 23:59:59, so
/// reading it as a boundary costs a theoretical second and buys the 87 %.
#[must_use]
pub fn firing_instant(valid_to: DateTime<Utc>, policy: &ReminderPolicy) -> DateTime<Utc> {
    let hms = (valid_to.hour(), valid_to.minute(), valid_to.second());
    if hms == (0, 0, 0) || hms == (23, 59, 59) {
        valid_to
            .with_day(valid_to.day())
            .and_then(|t| t.with_hour(policy.day_hour_utc.min(23)))
            .and_then(|t| t.with_minute(0))
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(valid_to)
    } else {
        valid_to - policy.lead
    }
}

/// Everyone a commitment rings for: its subject and the people it was
/// shared with, each as a `user:` principal.
///
/// A group is opened into its members — it has no inbox of its own, but they
/// have one each, which is what "a household commitment" means. An agent is
/// dropped: it has no inbox at all. `global` is dropped too, being everybody
/// and therefore nobody to ring. Order is stable and duplicates removed, so
/// the same person addressed twice rings once.
///
/// Best-effort by contract, like the sweep around it: a membership lookup
/// that fails leaves that principal out rather than failing the sweep.
async fn people_to_ring(pool: &SqlitePool, row: &fact_index::FactIndexRow) -> Vec<String> {
    use crate::types::Principal;
    let mut out: Vec<String> = Vec::new();
    let push = |id: &str, out: &mut Vec<String>| {
        let wire = format!("user:{id}");
        if !out.contains(&wire) {
            out.push(wire);
        }
    };
    for principal in std::iter::once(&row.subject_id).chain(row.allow_ids.iter()) {
        match principal {
            Principal::User(id) => push(id, &mut out),
            // The builtin global group parses as a `Group` whose membership
            // list is empty — everybody, and therefore nobody to put a
            // commitment in front of.
            Principal::Group(id) => {
                for member in crate::enrollment::members_for(pool, id)
                    .await
                    .unwrap_or_default()
                {
                    push(&member, &mut out);
                }
            },
        }
    }
    // An agent principal has no inbox — same rule as the fact-minted notice.
    let mut people = Vec::with_capacity(out.len());
    for wire in out {
        let id = wire.strip_prefix("user:").unwrap_or(&wire).to_owned();
        if !crate::enrollment::is_agent(pool, &id)
            .await
            .unwrap_or(false)
        {
            people.push(wire);
        }
    }
    people
}

/// The commitments whose firing instant has just passed.
///
/// Selection: an active, unclosed `plan` with a `valid_to` whose
/// [`firing_instant`] sits in `(now - grace, now]` and which has not already
/// rung.
///
/// Who it rings for is [`DueReminder::recipients`]: the subject and the
/// audience, groups opened into their members, agents dropped (they have no
/// inbox). A commitment nobody has an inbox for is skipped.
///
/// # Errors
///
/// [`crate::Error`] from the fact-index read; an events probe failure
/// surfaces the same way.
pub async fn due_now(
    pool: &SqlitePool,
    now: DateTime<Utc>,
    policy: &ReminderPolicy,
) -> crate::Result<Vec<DueReminder>> {
    if !policy.enabled {
        return Ok(Vec::new());
    }
    let fmt = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let window = chrono::Duration::hours(CANDIDATE_WINDOW_HOURS);
    let rows = fact_index::find_due_between(pool, &fmt(now - window), &fmt(now + window), 0)
        .await
        .map_err(|e| crate::Error::Other(format!("reminders: due scan: {e}")))?;

    let floor = now - policy.grace;
    let mut due = Vec::new();
    for row in rows {
        if due.len() >= policy.cap {
            break;
        }
        if row.fact_type.as_deref() != Some(PLAN) {
            continue;
        }
        // A closure wrote this `valid_to`, so it is the instant the plan
        // stopped holding and not a deadline anybody stated. Only the
        // classifier's own bound rings; see the module header.
        if row.decay_reason.is_some() {
            continue;
        }
        let Some(valid_to) = row.valid_to.as_deref() else {
            continue;
        };
        let Ok(parsed) = DateTime::parse_from_rfc3339(valid_to) else {
            tracing::warn!(
                fact_id = %row.fact_id,
                valid_to,
                "reminders: unparsable valid_to, skipped"
            );
            continue;
        };
        let fires_at = firing_instant(parsed.with_timezone(&Utc), policy);
        if fires_at > now || fires_at <= floor {
            continue;
        }
        let recipients = people_to_ring(pool, &row).await;
        if recipients.is_empty() {
            continue;
        }
        due.push(DueReminder {
            fact_id: row.fact_id.as_str().to_owned(),
            wiki_id: row.wiki_id.clone(),
            recipients,
            body: row.text.clone(),
            due_at: valid_to.to_owned(),
            fires_at,
        });
    }
    Ok(due)
}

/// Emit a `reminder_due` notice for every commitment that has come due.
///
/// The payload mirrors `fact_minted_for_you` — `recipient_id`, a `facts`
/// array carrying the body, a `dashboard_path` — so a consumer that
/// already delivers one delivers the other with the same parsing.
///
/// # Errors
///
/// [`crate::Error`] from the underlying reads; an event insert that fails
/// aborts the sweep (the next tick retries — the grace window is wider
/// than the tick).
pub async fn sweep(
    pool: &SqlitePool,
    now: DateTime<Utc>,
    policy: &ReminderPolicy,
) -> crate::Result<SweepReport> {
    let mut report = SweepReport::default();
    let due = due_now(pool, now, policy).await?;
    report.examined = due.len();
    for r in due {
        // One event per person: an event carries a single addressee, and
        // "has this already rung" is therefore a question about a person.
        // Asking it about the fact alone would ring the first of a household
        // and silence the rest.
        for recipient in &r.recipients {
            let rung = events::find_recent_event_for_recipient(
                pool,
                EventKind::ReminderDue,
                &r.fact_id,
                Some(recipient),
                chrono::Duration::days(ALREADY_RUNG_DAYS),
            )
            .await
            .map_err(|e| crate::Error::Other(format!("reminders: events probe: {e}")))?;
            if rung {
                report.already_rung += 1;
                continue;
            }
            let payload = serde_json::json!({
                "recipient_id": recipient,
                "due_at": r.due_at,
                "fires_at": r.fires_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "facts": [{
                    "fact_id": r.fact_id,
                    "wiki_id": r.wiki_id,
                    "body": r.body,
                }],
                "dashboard_path": format!("/dashboard/wiki/{}", r.wiki_id),
            });
            events::insert_event(
                pool,
                EventKind::ReminderDue,
                Some(&r.wiki_id),
                Some(&r.fact_id),
                &payload,
            )
            .await
            .map_err(|e| crate::Error::Other(format!("reminders: emit: {e}")))?;
            tracing::info!(
                fact_id = %r.fact_id,
                recipient = %recipient,
                due_at = %r.due_at,
                "reminders: commitment came due"
            );
            report.emitted.push(r.fact_id.clone());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    async fn fresh_pool() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    /// `FactId` refuses anything that is not a lowercase hyphenated
    /// `UUIDv7`, so fixtures mint real-shaped ids rather than labels.
    fn id(n: u8) -> String {
        format!("018f1234-5678-7abc-9def-0000000000{n:02}")
    }

    /// A fact row with the shape the sweep selects on. Returns its id.
    async fn plan_fact(
        pool: &SqlitePool,
        fact_id: &str,
        subject: &str,
        fact_type: &str,
        valid_to: Option<&str>,
    ) {
        plan_fact_shared(pool, fact_id, subject, fact_type, valid_to, "[]").await;
    }

    async fn plan_fact_shared(
        pool: &SqlitePool,
        fact_id: &str,
        subject: &str,
        fact_type: &str,
        valid_to: Option<&str>,
        allow_ids: &str,
    ) {
        sqlx::query(
            r#"INSERT INTO fact_index
                 (fact_id, wiki_id, source_path, "text", embedding, embedding_dim,
                  subject_id, allow_ids, fact_type, created_at, updated_at, valid_to)
               VALUES (?, 'alice', 'alice/cucina.md', 'dentist at nine', X'00000000', 1,
                       ?, ?, ?, '2026-07-01T00:00:00Z', '2026-07-01T00:00:00Z', ?)"#,
        )
        .bind(fact_id)
        .bind(subject)
        .bind(allow_ids)
        .bind(fact_type)
        .bind(valid_to)
        .execute(pool)
        .await
        .expect("insert fact");
    }

    async fn enrol(pool: &SqlitePool, users: &[&str], group: &str) {
        for u in users {
            sqlx::query("INSERT OR IGNORE INTO enrollment_users (user_id, aliases, is_admin) VALUES (?, '[]', 0)")
                .bind(u)
                .execute(pool)
                .await
                .expect("enrol");
        }
        let members = serde_json::to_string(users).expect("json");
        sqlx::query("INSERT OR REPLACE INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind(group)
            .bind(members)
            .execute(pool)
            .await
            .expect("group");
    }

    #[test]
    fn a_date_with_no_hour_fires_in_the_morning_not_at_midnight() {
        let p = ReminderPolicy::default();
        // The 87 % case, both spellings of "a date".
        assert_eq!(
            firing_instant(t("2026-08-11T00:00:00Z"), &p),
            t("2026-08-11T07:00:00Z")
        );
        assert_eq!(
            firing_instant(t("2026-08-11T23:59:59Z"), &p),
            t("2026-08-11T07:00:00Z")
        );
    }

    #[test]
    fn a_stated_time_fires_at_that_time_minus_the_lead() {
        let p = ReminderPolicy::default();
        assert_eq!(
            firing_instant(t("2026-08-11T17:00:00Z"), &p),
            t("2026-08-11T17:00:00Z")
        );
        let early = ReminderPolicy {
            lead: chrono::Duration::hours(2),
            ..ReminderPolicy::default()
        };
        assert_eq!(
            firing_instant(t("2026-08-11T17:00:00Z"), &early),
            t("2026-08-11T15:00:00Z")
        );
    }

    #[tokio::test]
    async fn only_a_dated_plan_with_somebody_to_ring_rings() {
        let (_workdir, pool) = fresh_pool().await;
        let now = t("2026-08-11T09:00:00Z");
        let day = Some("2026-08-11T23:59:59Z");
        plan_fact(&pool, id(1).as_str(), "user:alice", "plan", day).await;
        // A transient state expires, it does not fall due.
        plan_fact(&pool, id(2).as_str(), "user:alice", "state", day).await;
        // A standing directive never rings.
        plan_fact(&pool, id(3).as_str(), "user:alice", "rule", day).await;
        // Communal, and nobody enrolled in that group here: nobody to ring.
        plan_fact(&pool, id(4).as_str(), "group:famiglia", "plan", day).await;
        // The shopping-list shape the prompt guarantees: no horizon.
        plan_fact(&pool, id(5).as_str(), "user:alice", "plan", None).await;

        let due = due_now(&pool, now, &ReminderPolicy::default())
            .await
            .expect("due");
        assert_eq!(due.len(), 1, "exactly the dated personal plan");
        assert_eq!(due[0].fact_id, id(1));
        assert_eq!(due[0].recipients, vec!["user:alice".to_owned()]);
        assert_eq!(due[0].fires_at, t("2026-08-11T07:00:00Z"));
    }

    /// A commitment rings for everyone it was told to, not for its subject
    /// alone.
    ///
    /// The case, 2026-09-06: a household's commitments either rang for one
    /// person or — when the household itself was the subject — for nobody,
    /// on the reasoning that "a group has no single inbox". Its members have
    /// one each, and being told about an appointment is what being reminded
    /// of it is for. Measured on the live memory: of 135 due commitments, 16
    /// were silent and 112 reached one member of the family they belonged to.
    #[tokio::test]
    async fn a_commitment_rings_for_everyone_it_was_shared_with() {
        let (_workdir, pool) = fresh_pool().await;
        enrol(&pool, &["alice", "bob", "carol"], "famiglia").await;
        let now = t("2026-08-11T09:00:00Z");
        let day = Some("2026-08-11T23:59:59Z");

        // Alice's appointment, told to the household.
        plan_fact_shared(
            &pool,
            id(1).as_str(),
            "user:alice",
            "plan",
            day,
            r#"["group:famiglia"]"#,
        )
        .await;
        // And one that is the household's own.
        plan_fact(&pool, id(2).as_str(), "group:famiglia", "plan", day).await;

        let due = due_now(&pool, now, &ReminderPolicy::default())
            .await
            .expect("due");
        assert_eq!(due.len(), 2, "both come due");

        let shared = due.iter().find(|d| d.fact_id == id(1)).expect("alice's");
        assert_eq!(
            shared.recipients,
            vec![
                "user:alice".to_owned(),
                "user:bob".to_owned(),
                "user:carol".to_owned()
            ],
            "the subject first, then the household it was told to"
        );

        let communal = due
            .iter()
            .find(|d| d.fact_id == id(2))
            .expect("the household's");
        assert_eq!(
            communal.recipients,
            vec![
                "user:alice".to_owned(),
                "user:bob".to_owned(),
                "user:carol".to_owned()
            ],
            "a group has no inbox, but its members have one each"
        );

        // And the sweep puts one notice in front of each of them.
        let report = sweep(&pool, now, &ReminderPolicy::default())
            .await
            .expect("sweep");
        assert_eq!(report.emitted.len(), 6, "two commitments, three people");
        // Run again: nobody is rung twice for the same commitment.
        let again = sweep(&pool, now, &ReminderPolicy::default())
            .await
            .expect("sweep again");
        assert!(
            again.emitted.is_empty(),
            "already rung: {:?}",
            again.emitted
        );
        assert_eq!(again.already_rung, 6);
    }

    /// A plan a later plan closed is not a deadline arriving.
    ///
    /// The production shape (2026-09-06): "I want a bar of soap" is captured
    /// at 18:36 with an open horizon; twelve minutes later "I have picked a
    /// brand" closes it, and the closure stamps `valid_to = 18:48` with a
    /// `decay_reason`. That stamp is the whole difference between the two
    /// authors of a `valid_to`, so the sweep keys on it.
    #[tokio::test]
    async fn a_plan_closed_by_a_later_plan_is_silent_and_a_real_deadline_rings() {
        use crate::types::FactId;

        let (_workdir, pool) = fresh_pool().await;
        // Captured with no horizon, then closed by the refined plan.
        plan_fact(&pool, id(11).as_str(), "user:alice", "plan", None).await;
        let closed = FactId::parse(id(11).as_str()).expect("fact id");
        fact_index::close_validity(
            &pool,
            &closed,
            "2026-08-11T18:48:00Z",
            fact_index::decay::CONTRADICTED,
            None,
        )
        .await
        .expect("close")
        .expect("an active row to close");
        // And a deadline the speaker actually stated, at the same hour.
        plan_fact(
            &pool,
            id(12).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-11T18:48:00Z"),
        )
        .await;

        let due = due_now(&pool, t("2026-08-11T18:50:00Z"), &ReminderPolicy::default())
            .await
            .expect("due");
        let ids: Vec<&str> = due.iter().map(|d| d.fact_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![id(12).as_str()],
            "the stated deadline rings; the plan a closure stamped does not"
        );
    }

    #[tokio::test]
    async fn a_commitment_long_past_never_rings_late() {
        let (_workdir, pool) = fresh_pool().await;
        plan_fact(
            &pool,
            id(6).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-10T23:59:59Z"),
        )
        .await;
        // 26 hours after its firing instant: outside the six-hour grace.
        let due = due_now(&pool, t("2026-08-11T09:00:00Z"), &ReminderPolicy::default())
            .await
            .expect("due");
        assert!(
            due.is_empty(),
            "yesterday's appointment must not ring today — the guard that \
             stops a first run emitting the whole backlog"
        );
    }

    #[tokio::test]
    async fn a_commitment_still_ahead_waits() {
        let (_workdir, pool) = fresh_pool().await;
        plan_fact(
            &pool,
            id(7).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-11T17:00:00Z"),
        )
        .await;
        let due = due_now(&pool, t("2026-08-11T09:00:00Z"), &ReminderPolicy::default())
            .await
            .expect("due");
        assert!(due.is_empty());
    }

    #[tokio::test]
    async fn a_commitment_rings_once_however_many_ticks_cover_it() {
        let (_workdir, pool) = fresh_pool().await;
        plan_fact(
            &pool,
            id(8).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-11T23:59:59Z"),
        )
        .await;
        let now = t("2026-08-11T09:00:00Z");
        let policy = ReminderPolicy::default();

        let first = sweep(&pool, now, &policy).await.expect("sweep");
        assert_eq!(first.emitted, vec![id(8)]);

        // The grace window spans many ticks; the second must be silent.
        let second = sweep(&pool, now + chrono::Duration::minutes(1), &policy)
            .await
            .expect("sweep");
        assert!(second.emitted.is_empty());
        assert_eq!(second.already_rung, 1);
    }

    #[tokio::test]
    async fn the_notice_carries_the_body_and_addresses_the_subject() {
        let (_workdir, pool) = fresh_pool().await;
        plan_fact(
            &pool,
            id(9).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-11T23:59:59Z"),
        )
        .await;
        sweep(&pool, t("2026-08-11T09:00:00Z"), &ReminderPolicy::default())
            .await
            .expect("sweep");
        let (kind, payload): (String, String) =
            sqlx::query_as("SELECT kind, payload FROM wiki_events WHERE fact_id = ?")
                .bind(id(9))
                .fetch_one(&pool)
                .await
                .expect("event");
        assert_eq!(kind, "reminder_due");
        let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
        assert_eq!(v["recipient_id"], "user:alice");
        assert_eq!(v["facts"][0]["body"], "dentist at nine");
        assert_eq!(v["dashboard_path"], "/dashboard/wiki/alice");
    }

    #[tokio::test]
    async fn the_switch_silences_the_sweep() {
        let (_workdir, pool) = fresh_pool().await;
        plan_fact(
            &pool,
            id(10).as_str(),
            "user:alice",
            "plan",
            Some("2026-08-11T23:59:59Z"),
        )
        .await;
        let off = ReminderPolicy {
            enabled: false,
            ..ReminderPolicy::default()
        };
        let report = sweep(&pool, t("2026-08-11T09:00:00Z"), &off)
            .await
            .expect("sweep");
        assert!(report.emitted.is_empty());
    }
}
