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
//! `fact_type = "plan"` **with a concrete `valid_to`**. That pair is
//! already precise, and not by luck: the ingest prompt forbids the one
//! `plan` that must never ring — *"a shopping item is NOT a TTL, it is
//! closed later by a completing message, not by a timer"* — so a
//! consumable intention is stored with `valid_to: null` by construction. A
//! `state` ("in Berlin this week") expires rather than falls due, and a
//! `rule` never rings.
//!
//! The firing instant is **derived**, not stored (roadmap 8b, decided on
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
//! exactly right; per-user local resolution is future work, noted on the
//! roadmap beside the per-turn timezone item.
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
const ALREADY_RUNG_DAYS: i64 = 365;

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
    /// Addressee: the owner of the commitment, `user:`-prefixed.
    pub recipient_id: String,
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

/// The commitments whose firing instant has just passed.
///
/// Selection: an active `plan` with a `valid_to`, owned by a **person**
/// (`user:` — a group has no single inbox and an agent has no inbox at
/// all), whose [`firing_instant`] sits in `(now - grace, now]` and which
/// has not already rung.
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
        // A group has no single inbox, and the wire form of the builtin
        // global group is not even `group:`-prefixed — only a person rings.
        let crate::types::Principal::User(owner) = &row.owner_id else {
            continue;
        };
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
        // An agent principal has no inbox — same rule as the
        // fact-minted notice.
        if crate::enrollment::is_agent(pool, owner.as_str())
            .await
            .unwrap_or(false)
        {
            continue;
        }
        due.push(DueReminder {
            fact_id: row.fact_id.as_str().to_owned(),
            wiki_id: row.wiki_id.clone(),
            recipient_id: row.owner_id.to_string(),
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
        let rung = events::find_recent_event_for(
            pool,
            EventKind::ReminderDue,
            &r.fact_id,
            chrono::Duration::days(ALREADY_RUNG_DAYS),
        )
        .await
        .map_err(|e| crate::Error::Other(format!("reminders: events probe: {e}")))?;
        if rung {
            report.already_rung += 1;
            continue;
        }
        let payload = serde_json::json!({
            "recipient_id": r.recipient_id,
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
            recipient = %r.recipient_id,
            due_at = %r.due_at,
            "reminders: commitment came due"
        );
        report.emitted.push(r.fact_id);
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
        owner: &str,
        fact_type: &str,
        valid_to: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO fact_index
                 (fact_id, wiki_id, source_path, "text", embedding, embedding_dim,
                  owner_id, fact_type, created_at, updated_at, valid_to)
               VALUES (?, 'alice', 'alice/index.md', 'dentist at nine', X'00000000', 1,
                       ?, ?, '2026-07-01T00:00:00Z', '2026-07-01T00:00:00Z', ?)"#,
        )
        .bind(fact_id)
        .bind(owner)
        .bind(fact_type)
        .bind(valid_to)
        .execute(pool)
        .await
        .expect("insert fact");
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
    async fn only_a_dated_plan_owned_by_a_person_rings() {
        let (_workdir, pool) = fresh_pool().await;
        let now = t("2026-08-11T09:00:00Z");
        let day = Some("2026-08-11T23:59:59Z");
        plan_fact(&pool, id(1).as_str(), "user:alice", "plan", day).await;
        // A transient state expires, it does not fall due.
        plan_fact(&pool, id(2).as_str(), "user:alice", "state", day).await;
        // A standing directive never rings.
        plan_fact(&pool, id(3).as_str(), "user:alice", "rule", day).await;
        // Communal: no single inbox.
        plan_fact(&pool, id(4).as_str(), "group:famiglia", "plan", day).await;
        // The shopping-list shape the prompt guarantees: no horizon.
        plan_fact(&pool, id(5).as_str(), "user:alice", "plan", None).await;

        let due = due_now(&pool, now, &ReminderPolicy::default())
            .await
            .expect("due");
        assert_eq!(due.len(), 1, "exactly the dated personal plan");
        assert_eq!(due[0].fact_id, id(1));
        assert_eq!(due[0].recipient_id, "user:alice");
        assert_eq!(due[0].fires_at, t("2026-08-11T07:00:00Z"));
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
    async fn the_notice_carries_the_body_and_addresses_the_owner() {
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
