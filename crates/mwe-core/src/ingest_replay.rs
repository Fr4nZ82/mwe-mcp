//! Idempotence at the entrance — the same turn delivered twice is answered
//! once.
//!
//! A consumer redelivers a turn whenever it cannot tell whether the first
//! delivery landed: its container was killed mid-call, the network retried, a
//! voice note reached it transcribed twice. Nothing durable goes wrong when it
//! does — the write path files no duplicate and the reconciler ends nothing
//! twice — but the turn is not free either. It runs the classifier, the
//! reconciliation stage and the navigator again, two or three model calls, to
//! arrive at the answer it already produced.
//!
//! And the answer is usually the point of the re-delivery. The shopping-list
//! case that opened this: the first container died AFTER the list was written,
//! so what the person never received was the reply. A second run that files
//! nothing and says nothing is the wrong shape — the right one is to hand back
//! what the first turn answered, which is what this module keeps.
//!
//! **Only COMPLETED turns are repeated.** A row is written when a turn
//! finishes, so a duplicate that arrives while the first is still in flight
//! finds nothing here and proceeds normally; the write path's own guards are
//! what make that harmless, and they already do.
//!
//! **The window is short on purpose** ([`crate::ingest::IngestPolicy`]). Inside
//! it the same words from the same speaker are a re-delivery; outside it they
//! are somebody saying the same thing again, which is a turn of its own and is
//! answered as one.
//!
//! **And only the person's LAST turn is repeatable**, whichever surface it came
//! from: recording one drops the rest ([`record`]). A retry is always of the
//! call the consumer just made, so that costs nothing — and it is what stops a
//! repeat ever handing back an answer the conversation has since moved past.

use sqlx::SqlitePool;

use crate::capture_buffer::origin_fingerprint;
use crate::ingest::{DisambigCandidate, IngestRequest, IngestResponse, IntentKind, MessageRole};
use crate::types::FactId;

/// What makes two deliveries the same turn.
///
/// Four parts, and each answers a way two turns could be told apart:
///
/// - the **sender** and the **consumer**, because one person's words on
///   Telegram and the same words in the kitchen are two conversations;
/// - the **speaker**, because a consumer feeds its own reply back for
///   extraction on the same surface as the user's message, and without this
///   one could be served the other's answer;
/// - a **fingerprint of what the turn carried in** — the text, the media
///   riding it, and the disambiguation choice it answers. Those three are the
///   whole of what the turn asks the memory to act on; a re-delivery repeats
///   them exactly, while a photo re-sent under the same one-word caption does
///   not, and must not be answered with the first photo's reply.
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

/// The answer a completed turn with this key gave inside the window, with the
/// instant it finished.
///
/// `None` when there is none, when the window is switched off (`0`), or when
/// the stored answer cannot be read back — every one of which means "run the
/// turn", which is always safe.
///
/// **`now` is the WALL clock, never the turn's semantic one.** What this
/// measures is how long ago the engine answered, and a backlog replay that
/// re-lives a turn at its utterance time did not arrive a year ago. Reading
/// the semantic clock here would also let one such import prune every live row
/// on its way in.
///
/// Reading is cheap by construction: one point lookup on the primary key.
pub async fn replay_of(
    pool: &SqlitePool,
    key: &TurnKey,
    window_minutes: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(IngestResponse, String)> {
    if window_minutes == 0 {
        return None;
    }
    let cutoff = (now - chrono::Duration::minutes(i64::from(window_minutes))).to_rfc3339();
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT created_at, reply FROM ingest_replies \
         WHERE sender_id = ? AND consumer_id = ? AND author = ? AND turn_hash = ? \
           AND created_at >= ?",
    )
    .bind(&key.sender_id)
    .bind(&key.consumer_id)
    .bind(key.author.as_str())
    .bind(&key.fingerprint)
    .bind(&cutoff)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "ingest-replay: lookup failed — the turn runs normally");
        None
    });
    let (created_at, reply) = row?;
    match serde_json::from_str::<Wire>(&reply) {
        Ok(wire) => Some((wire.into_response(), created_at)),
        Err(e) => {
            tracing::warn!(error = %e, "ingest-replay: stored reply unreadable — the turn runs normally");
            None
        },
    }
}

/// Keep this turn's answer for a re-delivery, and drop what has aged out.
///
/// Best-effort in both halves: a turn that cannot record its answer is a turn
/// that will be run again if it arrives again, which is exactly the behaviour
/// this module replaces. It is never a reason to fail a turn that succeeded.
///
/// Both prunes ride the insert, the way [`crate::recent_window::record_exchange`]
/// bounds its own buffer — the sender's earlier turns, then everything past the
/// window — so the table cannot outgrow its contract even on a deployment where
/// nothing is ever redelivered.
///
/// `now` is the wall clock, for the reason spelled out on [`replay_of`].
pub async fn record(
    pool: &SqlitePool,
    key: &TurnKey,
    response: &IngestResponse,
    window_minutes: u32,
    now: chrono::DateTime<chrono::Utc>,
) {
    if window_minutes == 0 {
        return;
    }
    let reply = match serde_json::to_string(&Wire::of(response)) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "ingest-replay: answer not serialisable — not kept");
            return;
        },
    };
    // REPLACE, not INSERT: the same words said again after the window are a
    // new turn, and it is the new answer a further re-delivery must get.
    if let Err(e) = sqlx::query(
        "INSERT OR REPLACE INTO ingest_replies \
           (sender_id, consumer_id, author, turn_hash, created_at, reply) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.sender_id)
    .bind(&key.consumer_id)
    .bind(key.author.as_str())
    .bind(&key.fingerprint)
    .bind(now.to_rfc3339())
    .bind(&reply)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, "ingest-replay: answer not kept (a repeat would be re-run)");
        return;
    }
    // ONLY THE PERSON'S LAST TURN IS REPEATABLE, across every surface they
    // talk to. A re-delivery is always of the call the consumer just made, so
    // nothing is lost by it — and it is what keeps a repeat from ever handing
    // back an answer the conversation has moved past. «Cosa c'è sulla spesa?»,
    // then «aggiungi il pane», then the same question again is a sequence a
    // person really types, and inside a bare time window the third turn would
    // have been served the list without the bread. Here the second turn drops
    // the first one's row, so the third is a turn and finds the bread.
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
    // And a sender who stops talking leaves nothing behind either.
    let cutoff = (now - chrono::Duration::minutes(i64::from(window_minutes))).to_rfc3339();
    if let Err(e) = sqlx::query("DELETE FROM ingest_replies WHERE created_at < ?")
        .bind(&cutoff)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "ingest-replay: aged rows not pruned");
    }
}

/// The stored shape of an [`IngestResponse`].
///
/// Written out by hand rather than derived onto the public type: `FactId` and
/// `IntentKind` enforce their own invariants at construction, and a derive
/// would let a hand-edited row bypass them. The conversion back goes through
/// the same constructors every other caller uses, and a value that does not
/// pass them is dropped rather than trusted.
#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    intent: String,
    context_snippet: Option<String>,
    rules: Option<String>,
    suggested_seed: Option<String>,
    recent_window: Option<String>,
    capture_id: Option<String>,
    needs_disambig: bool,
    disambig_candidates: Vec<WireCandidate>,
    llm_used: bool,
    took_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireCandidate {
    candidate_id: String,
    description: String,
}

impl Wire {
    fn of(response: &IngestResponse) -> Self {
        Self {
            intent: response.intent.as_str().to_owned(),
            context_snippet: response.context_snippet.clone(),
            rules: response.rules.clone(),
            suggested_seed: response.suggested_seed.clone(),
            recent_window: response.recent_window.clone(),
            capture_id: response.capture_id.as_ref().map(|f| f.as_str().to_owned()),
            needs_disambig: response.needs_disambig,
            disambig_candidates: response
                .disambig_candidates
                .iter()
                .map(|c| WireCandidate {
                    candidate_id: c.candidate_id.clone(),
                    description: c.description.clone(),
                })
                .collect(),
            llm_used: response.llm_used,
            took_ms: response.took_ms,
        }
    }

    /// The answer as the consumer received it the first time — `took_ms` and
    /// `llm_used` included. They describe the turn that produced this reply,
    /// and this reply IS that turn's, handed over a second time.
    fn into_response(self) -> IngestResponse {
        IngestResponse {
            intent: IntentKind::parse(&self.intent).unwrap_or(IntentKind::Skip),
            context_snippet: self.context_snippet,
            rules: self.rules,
            suggested_seed: self.suggested_seed,
            recent_window: self.recent_window,
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
            llm_used: self.llm_used,
            took_ms: self.took_ms,
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

    fn answer() -> IngestResponse {
        IngestResponse {
            intent: IntentKind::Capture,
            context_snippet: Some("RELEVANT MEMORY\n- latte 2".to_owned()),
            rules: Some("Answer concisely.".to_owned()),
            suggested_seed: Some("Segnato.".to_owned()),
            recent_window: None,
            capture_id: Some(
                FactId::parse("018f1234-5678-7abc-9def-0123456789ab").expect("fact id"),
            ),
            needs_disambig: true,
            disambig_candidates: vec![DisambigCandidate {
                candidate_id: "c1".to_owned(),
                description: "the shopping list".to_owned(),
            }],
            llm_used: true,
            took_ms: 1234,
        }
    }

    /// What comes back is what went in — every field of it.
    ///
    /// The whole value of this table is that the second delivery gets the
    /// FIRST one's answer, so a field quietly lost on the way through is the
    /// bug it exists to prevent: a recall block, a seed, the capture id the
    /// consumer logs, the disambiguation the person is waiting to answer.
    #[tokio::test]
    async fn the_answer_comes_back_exactly_as_it_was_given() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("due litri di latte"));
        record(&pool, &key, &answer(), 10, now).await;

        let (back, _) = replay_of(&pool, &key, 10, now)
            .await
            .expect("the turn is repeatable");
        let first = answer();
        assert_eq!(back.intent, first.intent);
        assert_eq!(back.context_snippet, first.context_snippet);
        assert_eq!(back.rules, first.rules);
        assert_eq!(back.suggested_seed, first.suggested_seed);
        assert_eq!(back.recent_window, first.recent_window);
        assert_eq!(back.capture_id, first.capture_id);
        assert_eq!(back.needs_disambig, first.needs_disambig);
        assert_eq!(back.disambig_candidates, first.disambig_candidates);
        assert_eq!(back.llm_used, first.llm_used);
        assert_eq!(
            back.took_ms, first.took_ms,
            "the duration is the first turn's, because the answer is"
        );
    }

    /// What separates two deliveries of one turn from two turns.
    ///
    /// Each of these would be answered with somebody else's reply if the key
    /// dropped that part, and the photo case is the one that costs a fact: the
    /// same one-word caption under a second picture is a different turn, with a
    /// different thing to say about it.
    #[tokio::test]
    async fn only_the_same_turn_from_the_same_speaker_repeats() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("ecco"));
        record(&pool, &key, &answer(), 10, now).await;

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
                replay_of(&pool, &TurnKey::of(&other), 10, now)
                    .await
                    .is_none(),
                "{what} is a turn of its own"
            );
        }
    }

    /// The window is what tells a re-delivery from somebody saying the same
    /// thing again.
    ///
    /// Past it the words are a new turn and are answered as one — and the new
    /// answer is what a further re-delivery gets, which is why the row is
    /// replaced rather than added to.
    #[tokio::test]
    async fn the_same_words_after_the_window_are_a_new_turn() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("come sempre"));
        record(&pool, &key, &answer(), 10, now).await;

        let later = now + chrono::Duration::minutes(11);
        assert!(
            replay_of(&pool, &key, 10, later).await.is_none(),
            "eleven minutes on, the same words are a new turn"
        );

        let mut second = answer();
        second.suggested_seed = Some("Di nuovo.".to_owned());
        record(&pool, &key, &second, 10, later).await;
        let (back, _) = replay_of(&pool, &key, 10, later)
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
    /// This is what keeps a repeat from ever serving an answer the
    /// conversation has moved past: ask what is on the shopping list, add the
    /// bread, ask again — inside a bare time window that third turn would come
    /// back with the list as it was before the bread. Saying something else in
    /// between drops the first answer, so the question is asked again for
    /// real.
    #[tokio::test]
    async fn saying_something_else_makes_the_earlier_turn_unrepeatable() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let question = TurnKey::of(&request("cosa c'è sulla spesa?"));
        record(&pool, &question, &answer(), 10, now).await;
        assert!(replay_of(&pool, &question, 10, now).await.is_some());

        // The same person, on another surface, says something else.
        let elsewhere = TurnKey::of(&IngestRequest {
            consumer_id: Some("voice".to_owned()),
            ..request("aggiungi il pane")
        });
        record(&pool, &elsewhere, &answer(), 10, now).await;

        assert!(
            replay_of(&pool, &question, 10, now).await.is_none(),
            "the question is asked again for real, and finds the bread"
        );
        assert!(
            replay_of(&pool, &elsewhere, 10, now).await.is_some(),
            "the turn that just happened is still the repeatable one"
        );
        // Another person's turn is not touched by any of it.
        let bobs = TurnKey::of(&IngestRequest {
            sender_id: "bob".to_owned(),
            ..request("e le mie?")
        });
        record(&pool, &bobs, &answer(), 10, now).await;
        assert!(replay_of(&pool, &elsewhere, 10, now).await.is_some());
        assert!(replay_of(&pool, &bobs, 10, now).await.is_some());
    }

    /// A window of `0` switches the whole thing off: nothing is kept and
    /// nothing is served, so every delivery is a turn.
    #[tokio::test]
    async fn a_zero_window_keeps_nothing() {
        let pool = pool().await;
        let now = chrono::Utc::now();
        let key = TurnKey::of(&request("niente"));
        record(&pool, &key, &answer(), 0, now).await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_replies")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(rows, 0);
        assert!(replay_of(&pool, &key, 0, now).await.is_none());
    }
}
