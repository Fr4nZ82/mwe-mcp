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

use std::sync::Arc;

use sqlx::Row;
use sqlx::SqlitePool;

use crate::embedder::Embedder;
use crate::fact_index::Result;
use crate::llm::{CompletionRequest, LlmBackend};

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

// ---------------------------------------------------------------------------
// accorpare — two words that mean the same thing become one
// ---------------------------------------------------------------------------

/// How near two words must sit before the model is asked whether they are the
/// same thing.
///
/// Measured on a real vocabulary of 671 words: the variants that need merging
/// sit at 0.75–0.82 (`pressione` / `pressione arteriosa` 0.80, `renale` /
/// `funzione renale` 0.80, `finanziamento` / `finanziamento auto` 0.82), and
/// words of the same subject that must NOT merge sit around 0.55–0.65
/// (`nutrizione` / `idratazione` 0.65, `pressione` / `peso` 0.65). The gap is
/// wide, and the threshold sits inside it rather than at either edge — this
/// only decides who gets ASKED, and asking costs one line of a prompt.
pub const MERGE_THRESHOLD: f32 = 0.72;

/// What one night's merging did.
#[derive(Debug, Clone, Default)]
pub struct TopicMergeReport {
    /// Pairs that reached the model.
    pub examined: usize,
    /// `(loser, winner, facts moved)` for every pair the model confirmed.
    pub merged: Vec<(String, String, usize)>,
    /// Failures that did not stop the sweep.
    pub errors: Vec<String>,
}

const MERGE_SYSTEM: &str =
    "You are shown two words used to tag facts in one household's memory, with how
many facts carry each. Say whether they NAME THE SAME THING.

Yes only when one is a spelling, an inflection or a wordier form of the other,
so that a reader would never choose between them on purpose: `pressione` and
`pressione arteriosa`, `finanziamento` and `finanziamento auto`, `nutrizione`
and `alimentazione`.

No when they are two different things that happen to live in the same subject.
`nutrizione` and `idratazione` are both about food and drink and are NOT the
same word. `prezzo` and `garanzia` are both about buying a car. Merging those
loses the distinction the narrower word exists to make, and nothing gives it
back.

When you are unsure, answer no. A duplicate left standing costs one wasted
word; a wrong merge costs a distinction, silently, forever.

Answer with JSON and nothing else: {\"same\": true|false}";

/// Merges the vocabulary's near-duplicates: the vector proposes the pairs,
/// the model decides, and the loser's facts are rewritten to the winner.
///
/// **The winner is the better-attested word, never the shorter or the
/// prettier one.** A merge exists to make a word count for more, so the facts
/// move to whichever of the two already carries more of them; a tie goes
/// alphabetically, so two runs of the same night agree.
///
/// Prevention would be better than repair — a labeller shown the words already
/// near the fact would not coin the duplicate at all — but prevention needs a
/// vector lookup on the path of every turn, and this needs none: it runs once
/// a night, on words, and touches no turn.
pub async fn merge_near_duplicates(
    pool: &SqlitePool,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    cap: usize,
) -> Result<TopicMergeReport> {
    let mut report = TopicMergeReport::default();
    if cap == 0 {
        return Ok(report);
    }
    let words = count_words(pool).await?;
    if words.len() < 2 {
        return Ok(report);
    }

    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(words.len());
    for w in &words {
        match embedder.embed(&w.word).await {
            Ok(v) => vectors.push(v),
            Err(e) => {
                report
                    .errors
                    .push(format!("merge: cannot embed `{}`: {e}", w.word));
                return Ok(report);
            },
        }
    }

    // Every pair above the threshold, richest first: the pair that would move
    // the most facts is the one worth the night's first call.
    let mut pairs: Vec<(usize, usize, f32)> = Vec::new();
    for i in 0..words.len() {
        for j in (i + 1)..words.len() {
            let near = crate::recall::cosine_similarity(&vectors[i], &vectors[j]);
            if near >= MERGE_THRESHOLD {
                pairs.push((i, j, near));
            }
        }
    }
    pairs.sort_by_key(|(i, j, _)| std::cmp::Reverse(words[*i].facts.min(words[*j].facts)));

    // A word already merged away this night must not be argued about again:
    // its facts have moved and its count is stale.
    let mut settled: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, j, near) in pairs {
        if report.examined >= cap {
            break;
        }
        let (a, b) = (&words[i], &words[j]);
        if settled.contains(&a.word) || settled.contains(&b.word) {
            continue;
        }
        let (winner, loser) = if (b.facts, &a.word) > (a.facts, &b.word) {
            (b, a)
        } else {
            (a, b)
        };
        report.examined += 1;
        let prompt = format!(
            "WORD A: `{}` — {} facts\nWORD B: `{}` — {} facts\n",
            winner.word, winner.facts, loser.word, loser.facts
        );
        let request = CompletionRequest {
            prompt,
            system: Some(MERGE_SYSTEM.to_string()),
            max_tokens: Some(60),
            temperature: Some(0.0),
            stop: Vec::new(),
            images: Vec::new(),
            truncation_expected: false,
            cache_system: true,
        };
        let same = match llm.complete(request).await {
            Ok(r) => first_bool(&r.text, "same").unwrap_or(false),
            Err(e) => {
                report
                    .errors
                    .push(format!("merge: `{}`/`{}`: {e}", winner.word, loser.word));
                continue;
            },
        };
        if !same {
            continue;
        }
        match rewrite_word(pool, &loser.word, &winner.word).await {
            Ok(moved) => {
                settled.insert(loser.word.clone());
                tracing::info!(
                    loser = loser.word,
                    winner = winner.word,
                    moved,
                    near,
                    "rem: topic words merged"
                );
                report
                    .merged
                    .push((loser.word.clone(), winner.word.clone(), moved));
            },
            Err(e) => report
                .errors
                .push(format!("merge: rewrite `{}`: {e}", loser.word)),
        }
    }
    Ok(report)
}

/// Reads a `{"same": …}` reply, tolerant of fences and prose around it.
fn first_bool(raw: &str, key: &str) -> Option<bool> {
    let start = raw.find('{')?;
    let end = raw[start..].find('}')? + start;
    serde_json::from_str::<serde_json::Value>(&raw[start..=end])
        .ok()?
        .get(key)?
        .as_bool()
}

/// Rewrites `loser` to `winner` on every live fact carrying it, returning how
/// many facts moved.
///
/// A fact already carrying both collapses to one word, and that is correct
/// rather than a loss: the pair said the same thing twice, which is exactly
/// what the merge decided.
async fn rewrite_word(pool: &SqlitePool, loser: &str, winner: &str) -> Result<usize> {
    let rows = sqlx::query(
        "SELECT fact_id, topics FROM fact_index
          WHERE deleted_at IS NULL AND superseded_at IS NULL
            AND EXISTS (SELECT 1 FROM json_each(fact_index.topics) WHERE json_each.value = ?)",
    )
    .bind(loser)
    .fetch_all(pool)
    .await?;

    let mut moved = 0usize;
    for row in &rows {
        let id: String = row.get("fact_id");
        let current: Vec<String> =
            serde_json::from_str(&row.get::<String, _>("topics")).unwrap_or_default();
        let mut next: Vec<String> = Vec::with_capacity(current.len());
        for w in current {
            let w = if w == loser { winner.to_owned() } else { w };
            if !next.contains(&w) {
                next.push(w);
            }
        }
        sqlx::query("UPDATE fact_index SET topics = ? WHERE fact_id = ?")
            .bind(serde_json::to_string(&next).unwrap_or_else(|_| "[]".to_owned()))
            .bind(&id)
            .execute(pool)
            .await?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;
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

    // ---------- accorpare ----------

    /// A word the embedder puts on top of another, and the model confirms:
    /// the facts move to the better-attested one, and nothing else changes.
    #[tokio::test]
    async fn a_confirmed_pair_moves_its_facts_to_the_better_attested_word() {
        let pool = make_pool().await;
        // Two words in the whole vocabulary, so exactly one pair exists and
        // the model's answer is the only thing that can decide it.
        for i in 1..=3 {
            fact(&pool, &format!("{i:02}"), &["pressione"]).await;
        }
        fact(&pool, "04", &["pressione arteriosa"]).await;

        // A fixed embedding puts EVERY pair above the threshold, so the model
        // is the only thing deciding — which is the contract under test.
        let embedder: Arc<dyn Embedder> =
            Arc::new(FakeEmbedder::with_fixed_embedding("fake", vec![1.0, 0.0]));
        let llm = FakeLlmBackend::new("fake", r#"{"same": true}"#);
        let report = merge_near_duplicates(&pool, &embedder, &llm, 1)
            .await
            .unwrap();

        assert_eq!(report.examined, 1);
        assert_eq!(report.merged.len(), 1, "{report:?}");
        let (loser, winner, moved) = &report.merged[0];
        assert_eq!(winner, "pressione", "three facts beat one");
        assert_eq!(loser, "pressione arteriosa");
        assert_eq!(*moved, 1);

        let counts = count_words(&pool).await.unwrap();
        assert!(
            counts.iter().all(|w| w.word != "pressione arteriosa"),
            "{counts:?}"
        );
        assert_eq!(
            counts.iter().find(|w| w.word == "pressione").unwrap().facts,
            4,
            "the merged fact now carries the winner: {counts:?}"
        );
    }

    /// The model's refusal is the end of it. A near pair that means two
    /// different things keeps both words, and the vector's opinion does not
    /// override that — the distinction the narrower word makes is exactly what
    /// a wrong merge destroys, silently and for good.
    #[tokio::test]
    async fn a_refused_pair_keeps_both_words() {
        let pool = make_pool().await;
        fact(&pool, "01", &["nutrizione", "porzioni"]).await;
        fact(&pool, "02", &["idratazione", "acqua"]).await;
        let embedder: Arc<dyn Embedder> =
            Arc::new(FakeEmbedder::with_fixed_embedding("fake", vec![1.0, 0.0]));
        let llm = FakeLlmBackend::new("fake", r#"{"same": false}"#);
        let report = merge_near_duplicates(&pool, &embedder, &llm, 4)
            .await
            .unwrap();

        assert!(report.examined > 0, "pairs were offered: {report:?}");
        assert!(report.merged.is_empty(), "{report:?}");
        let counts = count_words(&pool).await.unwrap();
        assert!(counts.iter().any(|w| w.word == "nutrizione"));
        assert!(counts.iter().any(|w| w.word == "idratazione"));
    }

    /// The cap counts pairs that reach the MODEL, so one night cannot spend
    /// the whole vocabulary's worth of calls.
    #[tokio::test]
    async fn the_cap_bounds_the_calls_not_the_pairs_considered() {
        let pool = make_pool().await;
        for i in 1..=6 {
            fact(
                &pool,
                &format!("{i:02}"),
                &[&format!("w{i}"), &format!("n{i}")],
            )
            .await;
        }
        let embedder: Arc<dyn Embedder> =
            Arc::new(FakeEmbedder::with_fixed_embedding("fake", vec![1.0, 0.0]));
        let llm = FakeLlmBackend::new("fake", r#"{"same": false}"#);
        let report = merge_near_duplicates(&pool, &embedder, &llm, 2)
            .await
            .unwrap();
        assert_eq!(report.examined, 2, "{report:?}");
    }

    /// Nothing to merge is not an error, and costs no call.
    #[tokio::test]
    async fn a_vocabulary_of_one_word_is_left_alone() {
        let pool = make_pool().await;
        fact(&pool, "01", &["salute"]).await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let llm = FakeLlmBackend::new("fake", r#"{"same": true}"#);
        let report = merge_near_duplicates(&pool, &embedder, &llm, 4)
            .await
            .unwrap();
        assert_eq!(report.examined, 0);
        assert!(report.merged.is_empty());
    }
}
