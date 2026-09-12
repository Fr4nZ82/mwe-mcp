// SPDX-License-Identifier: AGPL-3.0-or-later
//! Idempotence at the entrance — the same turn delivered twice is **written**
//! once, and **read** twice.
//!
//! A consumer redelivers a turn whenever it cannot tell whether the first
//! delivery landed: its container was killed mid-call, the network retried, a
//! voice note reached it transcribed twice. Nothing durable goes wrong when it
//! does — the write path files no duplicate and the reconciler ends nothing
//! twice — but the turn is not free either. It runs the classifier and the
//! reconciliation stage again, the two expensive calls, to arrive at the
//! decisions it already made. And the answer is usually the point of the
//! re-delivery: the shopping-list case that opened this, where the first
//! container died AFTER the list was written, so what the person never
//! received was the reply.
//!
//! **What is kept is the WRITE half, and only that**: what the turn decided
//! and filed — the intent, the seed, the id of what it captured, the notices
//! it owes, the question it asked. The memory BLOCK is not kept and is never
//! replayed. A recall block is a projection of the memory as it stands, and
//! ten minutes is long enough for a fact to be forgotten, a permission to be
//! narrowed or a rule to be withdrawn: handing back the old block would put
//! the forgotten thing in front of the person again, and write it into the
//! trace journal with today's date. So a repeat re-reads — recall, the
//! navigator, the identity cards, the rules in force — and pays for that,
//! while the classifier and the reconciler do not run at all.
//!
//! **Only COMPLETED turns are repeated.** A row is written when a turn
//! finishes, so a duplicate that arrives while the first is still in flight
//! finds nothing here and proceeds normally; the write path's own guards are
//! what make that harmless, and they already do.
//!
//! **The window is short on purpose** ([`crate::ingest::IngestPolicy`]). Inside
//! it the same words from the same speaker are a re-delivery; outside it they
//! are somebody saying the same thing again, which is a turn of its own and is
//! answered as one. Each row carries its own `expires_at`, stamped from the
//! window in force when it was written, so the serving check, the write-path
//! prune and the light round's sweep ([`prune`]) agree without a knob between
//! them.
//!
//! **And only the person's LAST turn is repeatable**, whichever surface it came
//! from: recording one drops the rest ([`record`]). A retry is always of the
//! call the consumer just made, so that costs nothing — and it is one more
//! thing that stops a repeat handing back a decision the conversation has
//! since moved past.

use sqlx::SqlitePool;

use crate::capture_buffer::origin_fingerprint;
use crate::ingest::{DisambigCandidate, IngestRequest, IntentKind, MessageRole};
use crate::types::FactId;

/// What makes two deliveries the same turn.
///
/// Four parts, and each answers a way two turns could be told apart:
///
/// - the **sender** and the **consumer**, because one person's words on
///   Telegram and the same words in the kitchen are two conversations;
/// - the **speaker**, because a consumer feeds its own reply back for
///   extraction on the same surface as the user's message, and without this
///   one could be served the other's outcome;
/// - a **fingerprint of what the turn carried in** — the text, the media
///   riding it, and the disambiguation choice it answers. Those three are the
///   whole of what the turn asks the memory to act on; a re-delivery repeats
///   them exactly, while a photo re-sent under the same one-word caption does
///   not, and must not be answered with the first photo's outcome.
///
/// What is deliberately NOT in it: the conversation the consumer attached. A
/// retry may carry a window that has moved on by a turn, and that is the same
/// delivery, not a new one.
#[derive(Debug)]
pub struct TurnKey {
    sender_id: String,
    consumer_id: String,
    author: MessageRole,
    fingerprint: String,
}

impl TurnKey {
    /// The key of this delivery.
    #[must_use]
    pub fn of(request: &IngestRequest) -> Self {
        // NUL separates the parts: it cannot occur in any of them, so no
        // arrangement of a text and a catalog id can spell another turn's
        // input. `origin_fingerprint` does the hashing, as it does for a
        // buffered capture's origin — one fingerprint function in the engine,
        // not two that could disagree about what "the same message" is.
        let mut material = request.text.clone();
        for attachment in &request.attachments {
            material.push('\u{0}');
            material.push_str(attachment.catalog_id.as_str());
        }
        if let Some(choice) = &request.disambig_choice {
            material.push('\u{0}');
            material.push_str(choice);
        }
        Self {
            sender_id: request.sender_id.clone(),
            consumer_id: request.consumer_id.clone().unwrap_or_default(),
            author: request.author,
            fingerprint: origin_fingerprint(&material),
        }
    }
}

/// What a completed turn DID to the memory — the half a re-delivery must not
/// do again.
///
/// Everything here is a DECISION of that turn, not a view of the store: what
/// the engine read the message to be, what it filed, what it could not do with
/// it, what it asked back. None of it goes stale in ten minutes, because none
/// of it describes the memory — which is exactly why the recall block, which
/// does, is not in this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// What the engine read the message to be.
    pub intent: IntentKind,
    /// The seed the consumer was handed to compose its reply from.
    pub suggested_seed: Option<String>,
    /// The id of the row the turn filed, when it filed one.
    pub capture_id: Option<FactId>,
    /// Whether the turn asked the person a question back.
    pub needs_disambig: bool,
    /// The choices that question offered.
    pub disambig_candidates: Vec<DisambigCandidate>,
    /// The one-shot notices the turn owes the person — each a thing the engine
    /// did NOT do with what they said. They ride the `rules` channel beside
    /// the directives in force, which a repeat recomputes.
    pub notice: Option<String>,
    /// Whether the classifier answered on the turn that produced this.
    pub llm_used: bool,
}

/// The write outcome a completed turn with this key produced, with the instant
/// it finished.
///
/// `None` when there is none, when the row has expired, or when the stored
/// outcome cannot be read back — every one of which means "run the turn",
/// which is always safe.
///
/// **`now` is the WALL clock, never the turn's semantic one.** What this
/// measures is how long ago the engine answered, and a backlog replay that
/// re-lives a turn at its utterance time did not arrive a year ago.
///
/// Reading is cheap by construction: one point lookup on the primary key.
pub async fn replay_of(
    pool: &SqlitePool,
    key: &TurnKey,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(TurnOutcome, String)> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT created_at, reply FROM ingest_replies \
         WHERE sender_id = ? AND consumer_id = ? AND author = ? AND turn_hash = ? \
           AND expires_at > ?",
    )
    .bind(&key.sender_id)
    .bind(&key.consumer_id)
    .bind(key.author.as_str())
    .bind(&key.fingerprint)
    .bind(now.to_rfc3339())
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "ingest-replay: lookup failed — the turn runs normally");
        None
    });
    let (created_at, reply) = row?;
    match serde_json::from_str::<Wire>(&reply) {
        Ok(wire) => Some((wire.into_outcome(), created_at)),
        Err(e) => {
            tracing::warn!(error = %e, "ingest-replay: stored outcome unreadable — the turn runs normally");
            None
        },
    }
}

/// Keep this turn's write outcome for a re-delivery, and drop what has aged
/// out.
///
/// Best-effort in both halves: a turn that cannot record its outcome is a turn
/// that will be run again if it arrives again, which is exactly the behaviour
/// this module replaces. It is never a reason to fail a turn that succeeded.
///
/// Both prunes ride the insert, the way [`crate::recent_window::record_exchange`]
/// bounds its own buffer — the sender's earlier turns, then everything past its
/// expiry — so the table cannot outgrow its contract on a busy deployment; the
/// light round's [`prune`] is what covers an idle one.
///
/// `now` is the wall clock, for the reason spelled out on [`replay_of`].
pub async fn record(
    pool: &SqlitePool,
    key: &TurnKey,
    outcome: &TurnOutcome,
    window_minutes: u32,
    now: chrono::DateTime<chrono::Utc>,
) {
    if window_minutes == 0 {
        return;
    }
    let reply = match serde_json::to_string(&Wire::of(outcome)) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "ingest-replay: outcome not serialisable — not kept");
            return;
        },
    };
    let expires_at = (now + chrono::Duration::minutes(i64::from(window_minutes))).to_rfc3339();
    // REPLACE, not INSERT: the same words said again after the window are a
    // new turn, and it is the new outcome a further re-delivery must get.
    if let Err(e) = sqlx::query(
        "INSERT OR REPLACE INTO ingest_replies \
           (sender_id, consumer_id, author, turn_hash, created_at, expires_at, reply) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.sender_id)
    .bind(&key.consumer_id)
    .bind(key.author.as_str())
    .bind(&key.fingerprint)
    .bind(now.to_rfc3339())
    .bind(&expires_at)
    .bind(&reply)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, "ingest-replay: outcome not kept (a repeat would be re-run)");
        return;
    }
    // ONLY THE PERSON'S LAST TURN IS REPEATABLE, across every surface they
    // talk to. A re-delivery is always of the call the consumer just made, so
    // nothing is lost by it — and it is what keeps a repeat from ever handing
    // back a decision the conversation has moved past. «Cosa c'è sulla
    // spesa?», then «aggiungi il pane», then the same question again is a
    // sequence a person really types: the second turn drops the first one's
    // row, so the third is a turn of its own.
    if let Err(e) = sqlx::query(
        "DELETE FROM ingest_replies WHERE sender_id = ? \
           AND NOT (consumer_id = ? AND author = ? AND turn_hash = ?)",
    )
    .bind(&key.sender_id)
    .bind(&key.consumer_id)
    .bind(key.author.as_str())
    .bind(&key.fingerprint)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, "ingest-replay: the sender's earlier turns were not dropped");
    }
    prune(pool, now).await;
}

/// Drop every kept outcome past its expiry.
///
/// Called on the way into [`record`] and once per light round
/// ([`crate::dream::run_light`]) — the second is what a deployment nobody is
/// talking to needs, since the write-path prune only ever runs when somebody
/// writes. Without it the last turn of the day sat in the table until the next
/// one, whenever that came.
///
/// Best-effort and silent about the ordinary case: this is hygiene, and a
/// sweep that cannot run leaves rows that are already refused at serving time.
pub async fn prune(pool: &SqlitePool, now: chrono::DateTime<chrono::Utc>) -> u64 {
    match sqlx::query("DELETE FROM ingest_replies WHERE expires_at <= ?")
        .bind(now.to_rfc3339())
        .execute(pool)
        .await
    {
        Ok(done) => done.rows_affected(),
        Err(e) => {
            tracing::warn!(error = %e, "ingest-replay: expired rows not pruned");
            0
        },
    }
}

/// The stored shape of a [`TurnOutcome`].
///
/// Written out by hand rather than derived onto the public type: `FactId` and
/// `IntentKind` enforce their own invariants at construction, and a derive
/// would let a hand-edited row bypass them. The conversion back goes through
/// the same constructors every other caller uses, and a value that does not
/// pass them is dropped rather than trusted.
#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    intent: String,
    suggested_seed: Option<String>,
    capture_id: Option<String>,
    needs_disambig: bool,
    disambig_candidates: Vec<WireCandidate>,
    notice: Option<String>,
    llm_used: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireCandidate {
    candidate_id: String,
    description: String,
}

impl Wire {
    fn of(outcome: &TurnOutcome) -> Self {
        Self {
            intent: outcome.intent.as_str().to_owned(),
            suggested_seed: outcome.suggested_seed.clone(),
            capture_id: outcome.capture_id.as_ref().map(|f| f.as_str().to_owned()),
            needs_disambig: outcome.needs_disambig,
            disambig_candidates: outcome
                .disambig_candidates
                .iter()
                .map(|c| WireCandidate {
                    candidate_id: c.candidate_id.clone(),
                    description: c.description.clone(),
                })
                .collect(),
            notice: outcome.notice.clone(),
            llm_used: outcome.llm_used,
        }
    }

    fn into_outcome(self) -> TurnOutcome {
        TurnOutcome {
            intent: IntentKind::parse(&self.intent).unwrap_or(IntentKind::Skip),
            suggested_seed: self.suggested_seed,
            capture_id: self
                .capture_id
                .as_deref()
                .and_then(|id| FactId::parse(id).ok()),
            needs_disambig: self.needs_disambig,
            disambig_candidates: self
                .disambig_candidates
                .into_iter()
                .map(|c| DisambigCandidate {
                    candidate_id: c.candidate_id,
                    description: c.description,
                })
                .collect(),
            notice: self.notice,
            llm_used: self.llm_used,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ContextHint, IngestMetadata};

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
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

    fn request(text: &str) -> IngestRequest {
        IngestRequest {
            text: text.to_owned(),
            author: MessageRole::User,
            sender_id: "alice".to_owned(),
            consumer_id: Some("botdeploy".to_owned()),
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata::default(),
            attachments: Vec::new(),
        }
    }

    fn outcome() -> TurnOutcome {
        TurnOutcome {
            intent: IntentKind::Capture,
            suggested_seed: Some("Segnato.".to_owned()),
            capture_id: Some(
                FactId::parse("018f1234-5678-7abc-9def-0123456789ab").expect("fact id"),
            ),
            needs_disambig: true,
            disambig_candidates: vec![DisambigCandidate {
                candidate_id: "c1".to_owned(),
                description: "the shopping list".to_owned(),
            }],
            notice: Some("NOTE — the list was not opened.".to_owned()),
            llm_used: true,
        }
    }

    /// What comes back is what went in — every field of it.
    ///
    /// The value of this table is that the second delivery does not decide
    /// anything a second time, so a field quietly lost on the way through is
    /// the bug it exists to prevent: the seed, the capture id the consumer
    /// logs, the notice that says what was refused, the question the person is
    /// waiting to answer.
    #[tokio::test]
    async fn the_write_outcome_comes_back_exactly_as_it_was_decided() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("due litri di latte"));
        record(&pool, &key, &outcome(), 10, now).await;

        let (back, _) = replay_of(&pool, &key, now)
            .await
            .expect("the turn is repeatable");
        assert_eq!(back, outcome());
    }

    /// What separates two deliveries of one turn from two turns.
    ///
    /// Each of these would be answered with somebody else's outcome if the key
    /// dropped that part, and the photo case is the one that costs a fact: the
    /// same one-word caption under a second picture is a different turn, with a
    /// different thing to say about it.
    #[tokio::test]
    async fn only_the_same_turn_from_the_same_speaker_repeats() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("ecco"));
        record(&pool, &key, &outcome(), 10, now).await;

        for (what, other) in [
            ("other words", request("ecco qua")),
            (
                "another sender",
                IngestRequest {
                    sender_id: "bob".to_owned(),
                    ..request("ecco")
                },
            ),
            (
                "another consumer",
                IngestRequest {
                    consumer_id: Some("voice".to_owned()),
                    ..request("ecco")
                },
            ),
            (
                "the assistant's own turn",
                IngestRequest {
                    author: MessageRole::Assistant,
                    ..request("ecco")
                },
            ),
            (
                "a second photo under the same caption",
                IngestRequest {
                    attachments: vec![crate::ingest::IngestAttachment {
                        catalog_id: crate::types::CatalogId::parse("c-2026-09-12-photo-002.jpg")
                            .expect("catalog id"),
                        kind: "photo".to_owned(),
                        caption: None,
                        description: None,
                    }],
                    ..request("ecco")
                },
            ),
        ] {
            assert!(
                replay_of(&pool, &TurnKey::of(&other), now).await.is_none(),
                "{what} is a turn of its own"
            );
        }
    }

    /// The window is what tells a re-delivery from somebody saying the same
    /// thing again.
    ///
    /// Past it the words are a new turn and are written as one — and the new
    /// outcome is what a further re-delivery gets, which is why the row is
    /// replaced rather than added to.
    #[tokio::test]
    async fn the_same_words_after_the_window_are_a_new_turn() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("come sempre"));
        record(&pool, &key, &outcome(), 10, now).await;

        let later = now + chrono::Duration::minutes(11);
        assert!(
            replay_of(&pool, &key, later).await.is_none(),
            "eleven minutes on, the same words are a new turn"
        );

        let mut second = outcome();
        second.suggested_seed = Some("Di nuovo.".to_owned());
        record(&pool, &key, &second, 10, later).await;
        let (back, _) = replay_of(&pool, &key, later)
            .await
            .expect("the new turn is itself repeatable");
        assert_eq!(back.suggested_seed.as_deref(), Some("Di nuovo."));

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_replies")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(
            rows, 1,
            "one row per turn, replaced rather than accumulated"
        );
    }

    /// Only the person's LAST turn is repeatable, whichever surface it came
    /// from.
    ///
    /// One more thing that keeps a repeat from acting on a decision the
    /// conversation has moved past: saying something else in between drops the
    /// earlier outcome, so the earlier turn is decided again for real.
    #[tokio::test]
    async fn saying_something_else_makes_the_earlier_turn_unrepeatable() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let question = TurnKey::of(&request("cosa c'è sulla spesa?"));
        record(&pool, &question, &outcome(), 10, now).await;
        assert!(replay_of(&pool, &question, now).await.is_some());

        // The same person, on another surface, says something else.
        let elsewhere = TurnKey::of(&IngestRequest {
            consumer_id: Some("voice".to_owned()),
            ..request("aggiungi il pane")
        });
        record(&pool, &elsewhere, &outcome(), 10, now).await;

        assert!(
            replay_of(&pool, &question, now).await.is_none(),
            "the earlier turn is decided again for real"
        );
        assert!(
            replay_of(&pool, &elsewhere, now).await.is_some(),
            "the turn that just happened is still the repeatable one"
        );
        // Another person's turn is not touched by any of it.
        let bobs = TurnKey::of(&IngestRequest {
            sender_id: "bob".to_owned(),
            ..request("e le mie?")
        });
        record(&pool, &bobs, &outcome(), 10, now).await;
        assert!(replay_of(&pool, &elsewhere, now).await.is_some());
        assert!(replay_of(&pool, &bobs, now).await.is_some());
    }

    /// **A deployment nobody is talking to keeps nothing.**
    ///
    /// The write-path prune only runs when somebody writes, so the last turn
    /// of the day would otherwise sit in the table until the next one. The
    /// sweep is by the clock, so an idle install holds the outcome for the
    /// window and no longer.
    #[tokio::test]
    async fn an_idle_deployment_keeps_nothing_past_the_window() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        record(
            &pool,
            &TurnKey::of(&request("l'ultimo turno del giorno")),
            &outcome(),
            10,
            now,
        )
        .await;

        assert_eq!(
            prune(&pool, now + chrono::Duration::minutes(9)).await,
            0,
            "inside the window it is still servable, so it stays"
        );
        assert_eq!(prune(&pool, now + chrono::Duration::minutes(11)).await, 1);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_replies")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(rows, 0, "nothing of that turn is left");
    }

    /// A window of `0` switches the whole thing off: nothing is kept and
    /// nothing is served, so every delivery is a turn.
    #[tokio::test]
    async fn a_zero_window_keeps_nothing() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("niente"));
        record(&pool, &key, &outcome(), 0, now).await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_replies")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(rows, 0);
        assert!(replay_of(&pool, &key, now).await.is_none());
    }
}
