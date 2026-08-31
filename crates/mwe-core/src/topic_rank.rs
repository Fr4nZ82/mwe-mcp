// SPDX-License-Identifier: AGPL-3.0-or-later
//! Which words this memory turns out to be **about**.
//!
//! Every fact carries two words ([`crate::ingest::MAX_FACT_TOPICS`]): the
//! broad one and the narrow one. Nothing declares which words are the broad
//! ones — **a word is broad because facts hang off it**, and this module is
//! the only place that says so.
//!
//! ## Why the ranking is computed and never stored
//!
//! Promotion and demotion are not operations here. There is no row to flip
//! and no fact to rewrite: a word rises because more facts arrived carrying
//! it, and falls because another word passed it. A stored list would have to
//! be kept in step with the counts it is derived from, and every way of doing
//! that is a way of getting it wrong.
//!
//! It also means the two halves the founder asked for come for free: *«un
//! numero fisso di sedie, e nessuno cade se non c'è qualcuno che sale»*
//! (2026-08-30) is what a `take(CHAIRS)` over a sorted count already does, and
//! *«un argomento non ha una fine»* is what NOT having a decay rule already
//! does. A word that stops being used keeps every fact it ever had; it simply
//! stops being near the top.
//!
//! ## The pair is not a hierarchy
//!
//! Neither word contains the other, and the same word can be the broad one on
//! one fact and the narrow one on the next (founder, 2026-08-31: *«un
//! macrotopic non "contiene" microtopics raggruppati»*). So this module ranks
//! **words**, not levels: it reads both slots of every fact into one count.

use sqlx::Row;
use sqlx::SqlitePool;

use crate::fact_index::Result;

/// How many words are macrotopics at any moment.
///
/// A fixed number of chairs, and the whole of what "closed list" means here.
/// Sized from the corpus this was measured on: sixteen is where a household's
/// six weeks stop being subjects and start being details — the sixteenth word
/// carries fourteen facts, the seventeenth ten, and everything below it names
/// something INSIDE one of the sixteen (`finanziamento` under `auto`,
/// `allattamento` under `puericultura`).
///
/// **There is no minimum fact count, and adding one would be a mistake**
/// (founder, 2026-08-31: *«il pavimento non serve a nulla: anche se il primo
/// fatto genera un macrotopic, poi verrà spodestato»*). A young memory whose
/// first word takes a chair is not wrong — it is a memory that has heard one
/// thing — and the chair is taken back by whatever grows past it. A floor
/// would buy nothing and would hide the ranking for as long as it held.
pub const MACROTOPIC_SLOTS: usize = 16;

/// Words the ENGINE writes into the same column for its own bookkeeping, by
/// prefix. They are not topics: see [`crate::ingest`], where the classifier
/// is refused them on the way in. Counting them here would rank
/// `signpost-day:2026-07-14` — a fresh word every day — as a subject this
/// household discusses daily.
const ENGINE_PREFIXES: &[&str] = &["signpost-", "project-signpost"];

/// One word and how many live facts carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordCount {
    /// The word, as the classifier wrote it (already lowercased on the way in).
    pub word: String,
    /// Live facts carrying it in either slot.
    pub facts: usize,
}

/// Every topic word in the memory with its fact count, most-used first.
///
/// Ties break alphabetically so two words with the same count do not swap
/// places between calls — a list that reorders on every read cannot be shown
/// to anybody, and cannot be compared with yesterday's.
///
/// Deleted and superseded facts are left out: a word kept aloft by facts the
/// memory has retired describes what this household USED to talk about, and
/// the ranking's whole job is to say what it talks about.
pub async fn count_words(pool: &SqlitePool) -> Result<Vec<WordCount>> {
    let rows = sqlx::query(
        "SELECT json_each.value AS word, COUNT(*) AS facts
           FROM fact_index, json_each(fact_index.topics)
          WHERE fact_index.deleted_at IS NULL
            AND fact_index.superseded_at IS NULL
            AND json_each.value <> ''
          GROUP BY json_each.value",
    )
    .fetch_all(pool)
    .await?;

    let mut out: Vec<WordCount> = rows
        .iter()
        .filter_map(|r| {
            let word: String = r.get("word");
            let facts: i64 = r.get("facts");
            let keep = !ENGINE_PREFIXES.iter().any(|p| word.starts_with(p));
            keep.then(|| WordCount {
                word,
                facts: usize::try_from(facts).unwrap_or(0),
            })
        })
        .collect();
    out.sort_by(|a, b| b.facts.cmp(&a.facts).then_with(|| a.word.cmp(&b.word)));
    Ok(out)
}

/// The words holding a chair right now — this memory's macrotopics.
///
/// Empty only when the memory is: one fact in, one word is a macrotopic, and
/// it stops being one the moment sixteen better-attested words exist. Nothing
/// is written when that happens and no fact is touched — the ranking is
/// recomputed, and the word keeps every fact it ever had.
pub async fn macrotopics(pool: &SqlitePool) -> Result<Vec<WordCount>> {
    Ok(count_words(pool)
        .await?
        .into_iter()
        .take(MACROTOPIC_SLOTS)
        .collect())
}

/// How often a newly-written word is one the memory already had.
///
/// The number that decides whether the fine grain has started to group:
/// early on almost every word is a coinage, and the ranking is a list of
/// things said once. Reported over the most recent `sample` facts so it
/// answers "is it grouping NOW", not "has it ever".
///
/// `None` when there are not enough labelled facts to say anything.
pub async fn recent_reuse(pool: &SqlitePool, sample: usize) -> Result<Option<f32>> {
    let rows = sqlx::query(
        "SELECT fact_id, topics FROM fact_index
          WHERE deleted_at IS NULL AND superseded_at IS NULL
            AND topics IS NOT NULL AND topics <> '[]'
          ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    let per_fact: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            serde_json::from_str::<Vec<String>>(&r.get::<String, _>("topics")).unwrap_or_default()
        })
        .map(|ws| {
            ws.into_iter()
                .filter(|w| !w.is_empty() && !ENGINE_PREFIXES.iter().any(|p| w.starts_with(p)))
                .collect()
        })
        .collect();
    if per_fact.len() <= sample {
        return Ok(None);
    }
    let cutoff = per_fact.len() - sample;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let (mut reused, mut written) = (0usize, 0usize);
    for (i, words) in per_fact.iter().enumerate() {
        for w in words {
            if i >= cutoff {
                written += 1;
                if seen.contains(w.as_str()) {
                    reused += 1;
                }
            }
            seen.insert(w.as_str());
        }
    }
    if written == 0 {
        return Ok(None);
    }
    // Both counts are word occurrences over a bounded sample — thousands at
    // the very most — so a `u16` holds either and the conversion to f32 is
    // exact. Saturating rather than wrapping: a sample big enough to overflow
    // would report 1.0, not a wrong small number.
    let ratio = f32::from(u16::try_from(reused).unwrap_or(u16::MAX))
        / f32::from(u16::try_from(written).unwrap_or(u16::MAX));
    Ok(Some(ratio))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

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

    /// Inserts one live fact carrying `words`. Only the columns this module
    /// reads are meaningful; the rest are the minimum the table demands.
    async fn fact(pool: &SqlitePool, id: &str, words: &[&str]) {
        sqlx::query(
            "INSERT INTO fact_index (fact_id, wiki_id, source_path, \"text\", embedding, \
             embedding_dim, subject_id, topics, created_at, updated_at) \
             VALUES (?, 'alice', 'wikis/alice/n.md', 'x', X'00', 1, 'user:alice', ?, ?, ?)",
        )
        .bind(id)
        .bind(serde_json::to_string(words).unwrap())
        .bind(format!("2026-01-{id:0>2}T00:00:00Z"))
        .bind("2026-01-01T00:00:00Z")
        .execute(pool)
        .await
        .expect("insert");
    }

    #[tokio::test]
    async fn a_word_is_counted_in_either_slot() {
        let pool = make_pool().await;
        fact(&pool, "01", &["salute", "nausea"]).await;
        fact(&pool, "02", &["nausea", "rimedio"]).await;
        let counts = count_words(&pool).await.unwrap();
        let nausea = counts.iter().find(|w| w.word == "nausea").unwrap();
        assert_eq!(
            nausea.facts, 2,
            "broad on one fact and narrow on the other is the same word: {counts:?}"
        );
    }

    /// The chairs are held by count and by nothing else. A word that arrived
    /// first holds one until better-attested words fill every chair — which
    /// is why no minimum is needed: the ranking corrects itself, and nothing
    /// is rewritten when it does.
    #[tokio::test]
    async fn the_first_word_holds_a_chair_and_loses_it_to_whatever_grows_past_it() {
        let pool = make_pool().await;
        fact(&pool, "01", &["cucina", "ghiaccioli"]).await;
        let first = macrotopics(&pool).await.unwrap();
        assert_eq!(
            first[0].word, "cucina",
            "one fact in, one macrotopic: {first:?}"
        );

        // Sixteen words with two facts each — every chair claimed by
        // something better attested than the one word that arrived first.
        let mut id = 2;
        for w in 0..MACROTOPIC_SLOTS {
            for _ in 0..2 {
                fact(&pool, &format!("{id:02}"), &[&format!("area{w:02}")]).await;
                id += 1;
            }
        }
        let now = macrotopics(&pool).await.unwrap();
        assert_eq!(now.len(), MACROTOPIC_SLOTS, "{now:?}");
        assert!(
            now.iter().all(|w| w.word != "cucina"),
            "the word that arrived first was dethroned, and its fact still has it: {now:?}"
        );
        let all = count_words(&pool).await.unwrap();
        assert!(
            all.iter().any(|w| w.word == "cucina" && w.facts == 1),
            "dethroned is not deleted: {all:?}"
        );
    }

    /// The engine's own bookkeeping lives in the same column and is not a
    /// subject. `signpost-day:` in particular coins a word a day, which would
    /// otherwise walk to the top of the ranking and stay there.
    #[tokio::test]
    async fn the_engines_markers_never_reach_the_ranking() {
        let pool = make_pool().await;
        for i in 1..=5 {
            fact(
                &pool,
                &format!("{i}"),
                &["signpost-description", "project-signpost"],
            )
            .await;
        }
        assert!(count_words(&pool).await.unwrap().is_empty());
    }

    /// A retired fact stops holding its word up: the ranking says what this
    /// memory talks about, not what it used to.
    #[tokio::test]
    async fn a_superseded_fact_stops_counting() {
        let pool = make_pool().await;
        for (i, narrow) in ["prezzo", "garanzia", "rata", "usato"].iter().enumerate() {
            fact(&pool, &format!("{}", i + 1), &["auto", narrow]).await;
        }
        let before = macrotopics(&pool).await.unwrap();
        assert_eq!(before[0].word, "auto");
        assert_eq!(before[0].facts, 4);

        sqlx::query("UPDATE fact_index SET superseded_at = '2026-02-01T00:00:00Z'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            macrotopics(&pool).await.unwrap().is_empty(),
            "nothing live is left"
        );
    }

    /// Reuse is the number that says whether the words have started to group.
    #[tokio::test]
    async fn reuse_counts_how_often_a_new_word_was_already_known() {
        let pool = make_pool().await;
        // Four facts of history, then two that reuse one word each.
        for (i, w) in [["a", "b"], ["c", "d"], ["e", "f"], ["g", "h"]]
            .iter()
            .enumerate()
        {
            fact(&pool, &format!("{}", i + 1), w).await;
        }
        fact(&pool, "5", &["a", "nuovo"]).await;
        fact(&pool, "6", &["c", "altro"]).await;
        let reuse = recent_reuse(&pool, 2)
            .await
            .unwrap()
            .expect("enough history");
        assert!(
            (reuse - 0.5).abs() < 1e-6,
            "2 of the last 4 words were known: {reuse}"
        );
    }

    /// Not enough history is not zero reuse — it is no answer.
    #[tokio::test]
    async fn reuse_says_nothing_when_there_is_no_history() {
        let pool = make_pool().await;
        fact(&pool, "01", &["a", "b"]).await;
        assert_eq!(recent_reuse(&pool, 10).await.unwrap(), None);
    }
}
