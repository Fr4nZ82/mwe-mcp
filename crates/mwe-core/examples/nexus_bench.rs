// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bench for the fact-and-nexus design: build a typed graph over a corpus of
//! facts and ask whether it retrieves better than flat vector search.
//!
//! The design says a memory is facts plus **nexuses** between them — an arc
//! carrying a type, a direction, the sentence that expresses the relation, and
//! that sentence's vector — and **nodes**, relation sentences nobody uttered
//! that several facts gather around. Neither exists in the engine. This
//! harness builds both outside it, over a read-only corpus database, so the
//! behavioural questions get answers before any engine code moves.
//!
//! ```text
//! cargo run -p mwe-core --example nexus_bench --features local-embedder --release -- \
//!     <subcommand> [options]
//!
//! topics      derive the installation's closed macrotopic list, then label
//!             every fact with a macrotopic and a free topic
//! candidates  pick, per fact, the facts a model should be asked about
//! weave       ask a model whether each pair holds a nexus, and of what type
//! judge       re-read every proposed nexus adversarially, one call each
//! nodes       write a relation sentence over each cluster of nexuses
//! ask         answer a query flat over facts, then over facts + nodes + walk
//! eval        score a gold set: coverage flat, and flat plus the macrotopic hop
//! vocab       does the vocabulary of topic words separate synonyms by vector?
//! ```
//!
//!
//! Two ways to reach a model, chosen with `--via`:
//!
//! - `cli` (default) — shells out to `claude -p`, which rides the operator's
//!   Claude Code subscription. Flat-rate, and it touches no token store, so it
//!   cannot rotate a credential a server is holding.
//! - `api` — `ANTHROPIC_API_KEY` through the engine's own backend. **A
//!   `sk-ant-api…` key here bills per token**, which is why it is not the
//!   default. Point it at a `claude setup-token`, never at a running server's
//!   login store: two refreshers over one refresh token invalidate each other.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::single_match_else,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    reason = "measurement harness: the report code is written for a human \
              reading numbers once, not for reuse"
)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use candle_core::Device;
use mwe_core::embedder::Embedder;
use mwe_core::fact_index::decode_embedding;
use mwe_core::llm::{AnthropicApiKey, AnthropicBackend, CompletionRequest, LlmBackend, LlmError};
use mwe_core::local_embedder::LocalEmbedder;
use mwe_core::recall::cosine_similarity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;

/// Where the bundled embedder's weights already sit on a machine that has
/// run the engine once.
const DEFAULT_MODEL_DIR: &str = "models/bge-m3";

/// Env var carrying the Anthropic credential. Classified by value: a
/// subscription token rides the flat plan, an `sk-ant-api…` Console key
/// bills per token.
const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Attempts per model call before the item is written off. At any useful
/// concurrency a run meets the provider's rate limiter, and a sample that
/// silently loses its slow half reads like a result.
const MAX_ATTEMPTS: u32 = 5;

/// First back-off step, doubled per attempt.
const BACKOFF_BASE_MS: u64 = 1_000;

/// Model calls in flight when `--width` says nothing. A headless `claude -p`
/// spends most of a call on session setup rather than on tokens, so the
/// useful width is set by how many node processes the machine tolerates, not
/// by the provider's rate limiter.
const CONCURRENCY: usize = 12;

// ---------------------------------------------------------------------------
// The eight nexus types
// ---------------------------------------------------------------------------

/// The vocabulary a nexus may carry, and the whole of it. Every entry was
/// read off an observed move in a recall trace, not invented to round out a
/// taxonomy — which is why there is no "see also": a category that applies
/// to every pair is chosen for every pair.
///
/// The type is **not** the meaning. The meaning is the sentence, and it is
/// the sentence that gets a vector; the type exists only so an arc can be
/// walked in one direction rather than both.
const NEXUS_TYPES: &[(&str, &str)] = &[
    ("causa", "A brings B about, or makes B more likely."),
    ("rimedio", "B resolves, treats or works around A."),
    ("vincolo", "A must hold for B to be possible; A bounds B."),
    (
        "contrasto",
        "B is the same kind of thing as A but differs in a way that matters.",
    ),
    (
        "successione",
        "B follows A in a course of events; order is the point, not mere dates.",
    ),
    (
        "sostituzione",
        "B takes A's place: A stopped being the case when B started.",
    ),
    (
        "caso di",
        "A is one instance of the general thing B, or of the recurring situation B.",
    ),
    (
        "ruolo",
        "A stands in a named relation to B: parent, employer, owner, partner.",
    ),
];

// ---------------------------------------------------------------------------
// Candidate quotas — the five roads a candidate can arrive by
// ---------------------------------------------------------------------------

/// Nearest by vector. The road that already exists in the engine, and the
/// only one that finds a pair sharing no subject, no day and no topic.
const QUOTA_NEAR: usize = 8;
/// Nearest among the facts of the same day and its neighbours. Causation is
/// usually said close in time to what it explains, and the vector cannot see
/// time at all.
const QUOTA_DAY: usize = 4;
/// Nearest among the facts about the same person or the same named thing.
const QUOTA_SUBJECT: usize = 4;
/// Nearest among the facts sharing a **macrotopic** but carrying a
/// DIFFERENT topic. The road that needs no likeness at all: four sentences
/// of the shape "X takes Y at hour Z" sit on top of each other in vector
/// space and none of them finds the others, yet they share one macrotopic
/// and differ in topic, which is exactly what says they belong together and
/// are not the same fact twice.
const QUOTA_MACRO: usize = 4;
/// Nearest among the facts carrying the SAME fine topic. Narrow by
/// construction, so it rarely fills its quota, and what it does find is the
/// tightest pairing any road offers.
const QUOTA_TOPIC: usize = 2;
/// Drawn at random from everything else. The true nexus between today's fact
/// and one from two years ago shares no word, no subject and no day; no cheap
/// signal finds it, and only a road that ignores every signal ever can.
const QUOTA_CHANCE: usize = 2;

/// Days either side counted as "the same day" for [`QUOTA_DAY`].
const DAY_WINDOW: i64 = 1;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One corpus fact, with everything the six roads need to rank it.
#[derive(Clone)]
struct Fact {
    id: String,
    text: String,
    kind: String,
    subject: String,
    subject_external: Option<String>,
    day: String,
    vector: Vec<f32>,
}

impl Fact {
    /// How the fact is shown to a model: enough context to judge a relation,
    /// no more. The id rides along because the model answers by id.
    fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut line = format!("[{}] ({}, {}) {}", self.id, self.kind, self.day, self.text);
        if let Some(ext) = &self.subject_external {
            let _ = write!(line, " · about: {ext}");
        }
        line
    }
}

/// What a fact is about, at two grains. The coarse one comes from a list
/// closed at labelling time; the fine one is free text.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Label {
    fact: String,
    #[serde(default)]
    macrotopic: String,
    /// The fine grain. `topic` is accepted on the wire so a label file
    /// written before the name settled still loads.
    #[serde(default)]
    #[serde(alias = "topic")]
    microtopic: String,
}

/// A fact and the facts a model will be asked about it.
#[derive(Serialize, Deserialize)]
struct Batch {
    fact: String,
    candidates: Vec<Candidate>,
}

/// One offered pair, tagged with the road it arrived by so the bench can
/// report which road paid for itself.
#[derive(Serialize, Deserialize, Clone)]
struct Candidate {
    id: String,
    road: String,
    cosine: f32,
}

/// A nexus as the weaving model proposed it.
#[derive(Serialize, Deserialize, Clone)]
struct Nexus {
    /// The fact whose batch produced this nexus. Kept apart from
    /// [`Self::from`], which the model may flip to the candidate when the
    /// relation runs the other way — a resume that keyed on `from` would
    /// re-buy every flipped batch.
    batch: String,
    from: String,
    to: String,
    #[serde(rename = "type")]
    kind: String,
    sentence: String,
    /// The road the candidate arrived by, carried through so a judged
    /// nexus can be attributed to the road that offered it.
    #[serde(default)]
    road: String,
}

/// One adversarial verdict on one nexus.
#[derive(Serialize, Deserialize)]
struct Verdict {
    from: String,
    to: String,
    #[serde(rename = "type")]
    kind: String,
    holds: bool,
    why: String,
    #[serde(default)]
    road: String,
}

/// A relation sentence written over a cluster, with its vector.
#[derive(Serialize, Deserialize)]
struct Node {
    sentence: String,
    covers: Vec<String>,
    #[serde(default)]
    vector: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        },
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        return Err(usage());
    };
    let opts = parse_opts(&args[1..])?;

    match cmd.as_str() {
        "topics" => cmd_topics(&opts).await,
        "candidates" => cmd_candidates(&opts).await,
        "weave" => cmd_weave(&opts).await,
        "judge" => cmd_judge(&opts).await,
        "nodes" => cmd_nodes(&opts).await,
        "ask" => cmd_ask(&opts).await,
        "eval" => cmd_eval(&opts).await,
        "vocab" => cmd_vocab(&opts).await,
        other => Err(format!("unknown subcommand `{other}`\n\n{}", usage())),
    }
}

fn usage() -> String {
    "usage: nexus_bench <topics|candidates|weave|judge|nodes|ask|eval|vocab> [--k v ...]"
        .to_string()
}

/// `--key value` pairs, the only shape this harness needs.
fn parse_opts(args: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let key = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected `--key`, got `{}`", args[i]))?;
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("`--{key}` wants a value"))?;
        out.insert(key.to_string(), value.clone());
        i += 2;
    }
    Ok(out)
}

/// Model calls in flight, from `--width` or [`CONCURRENCY`].
fn width(o: &HashMap<String, String>) -> usize {
    o.get("width")
        .and_then(|w| w.parse().ok())
        .filter(|w| *w > 0)
        .unwrap_or(CONCURRENCY)
}

fn opt<'a>(o: &'a HashMap<String, String>, k: &str) -> Result<&'a str, String> {
    o.get(k)
        .map(String::as_str)
        .ok_or_else(|| format!("`--{k}` is required"))
}

// ---------------------------------------------------------------------------
// Loading the corpus
// ---------------------------------------------------------------------------

/// Reads the live facts out of a corpus database. Deleted and superseded
/// rows are left behind: a nexus onto a fact the engine has retired would
/// be measured as a hit nobody could ever follow.
async fn load_facts(db: &Path, before: Option<&str>) -> Result<Vec<Fact>, String> {
    let url = format!("sqlite://{}?mode=ro", db.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .map_err(|e| format!("cannot open {}: {e}", db.display()))?;

    // The corpus databases this bench reads span two schemas: the engine
    // renamed the responsible principal from `owner_id` to `subject_id` and
    // added `subject_external` later. Both carry the same fact, so the
    // loader asks the database which shape it is instead of refusing the
    // older one — which is the only complete engine there is to measure
    // against.
    let columns: HashSet<String> = sqlx::query("SELECT name FROM pragma_table_info('fact_index')")
        .fetch_all(&pool)
        .await
        .map_err(|e| format!("cannot read the schema: {e}"))?
        .into_iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    let subject = if columns.contains("subject_id") {
        "subject_id"
    } else {
        "owner_id"
    };
    let external = if columns.contains("subject_external") {
        "subject_external"
    } else {
        "NULL AS subject_external"
    };
    let sql = format!(
        "SELECT fact_id, \"text\", fact_type, {subject} AS subject_id, {external}, \
         created_at, embedding \
         FROM fact_index \
         WHERE deleted_at IS NULL AND superseded_at IS NULL \
         ORDER BY created_at"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&pool)
        .await
        .map_err(|e| format!("query failed: {e}"))?;

    let mut facts = Vec::new();
    for row in rows {
        let created: String = row.get("created_at");
        if let Some(cut) = before
            && created.as_str() >= cut
        {
            continue;
        }
        let blob: Vec<u8> = row.get("embedding");
        let vector = decode_embedding(&blob).map_err(|e| format!("bad embedding: {e}"))?;
        facts.push(Fact {
            id: row.get("fact_id"),
            text: row.get("text"),
            kind: row
                .get::<Option<String>, _>("fact_type")
                .unwrap_or_default(),
            subject: row.get("subject_id"),
            subject_external: row.get("subject_external"),
            day: created.chars().take(10).collect(),
            vector,
        });
    }
    Ok(facts)
}

/// Whole days between two `YYYY-MM-DD` strings, unsigned.
fn day_gap(left: &str, right: &str) -> i64 {
    // Days since an arbitrary epoch: the civil-from-days algorithm, shifted to
    // March-based years so the leap day lands at the end. Exact enough for a
    // window of one, and it needs no calendar crate.
    let parse = |text: &str| -> Option<i64> {
        let mut parts = text.split('-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        let shifted = if month <= 2 { year - 1 } else { year };
        let era = shifted.div_euclid(400);
        let year_of_era = shifted - era * 400;
        let month_shifted = (month + 9) % 12;
        let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        Some(era * 146_097 + day_of_era)
    };
    match (parse(left), parse(right)) {
        (Some(a), Some(b)) => (a - b).abs(),
        _ => i64::MAX,
    }
}

// ---------------------------------------------------------------------------
// topics — a closed coarse list, a free fine one
// ---------------------------------------------------------------------------

/// Macrotopics asked for. Small enough that a bucket groups something, wide
/// enough that the labeller is not forced to lie: at 980 distinct values
/// over 1367 facts the field the engine has today groups nothing at all.
const MACROTOPIC_TARGET: usize = 16;

/// Facts per labelling call. Big enough to amortise the list in the system
/// prompt, small enough that the model still reads each one.
const LABEL_BATCH: usize = 10;

const LIST_SYSTEM: &str = "\
You are naming the coarse subjects ONE household's memory is made of.

You are given the facts that memory holds. Propose a closed list of about
<<n>> macrotopics that covers them — the buckets somebody would sort these
facts into if they had to, and could use again next year.

A macrotopic is a SUBJECT AREA. It is never a person's name and never one
object's name: those already have their own place in this memory. `salute`
is a macrotopic; `bob` and `the-blue-estate` are not.

Aim for buckets that each hold several facts. A bucket holding one fact is
a wasted name, and so is a bucket so wide that half the memory lands in it.
Use single lowercase words in the language of the facts.

Answer with JSON and nothing else:

{\"macrotopics\": [\"...\", \"...\"]}";

const LABEL_SYSTEM: &str = "\
You label facts in a memory at two grains.

The MACROTOPIC is the coarse subject area, and it must come from this list
and nowhere else:

<<list>>

If no entry fits, answer `altro` — that is a real answer and it leaves a
trace somebody can act on later. Never invent an entry.

The TOPIC is the fine one: a word or two, free, saying which particular
thing inside the macrotopic this fact is about. `salute` / `nausea`.
`integratori` / `orario`. The point of the pair is that facts sharing a
macrotopic and differing in topic BELONG TOGETHER AND ARE NOT THE SAME
FACT — so make the topic the thing that distinguishes this fact from its
neighbours, not a synonym of the macrotopic.

Two kinds of fact have no subject area: one that says WHO SOMEBODY IS, and
one that states a standing instruction about how this memory or its
assistant should behave. For those return an empty macrotopic and an empty
topic.

Answer with JSON and nothing else, one entry per fact you were given:

{\"labels\": [{\"id\": \"<fact id>\", \"macrotopic\": \"...\", \"topic\": \"...\"}]}";

/// Derives the installation's macrotopic list, then labels every fact with
/// one entry from it plus a free fine topic.
///
/// The list is closed **at labelling time** and derived from the corpus
/// rather than shipped with the product: a print shop, a family nursing a
/// relative and a student do not share subject areas, and a list written by
/// whoever built the engine would fit none of them.
async fn cmd_topics(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let out = PathBuf::from(opt(o, "out")?);

    let lister = Caller::build(o, "claude-opus-5")?;
    let list: Vec<String> = if let Some(path) = o.get("list") {
        fs::read_to_string(path)
            .map_err(|e| format!("cannot read {path}: {e}"))?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        let corpus: Vec<String> = facts.iter().map(|f| format!("- {}", f.text)).collect();
        let system = LIST_SYSTEM.replace("<<n>>", &MACROTOPIC_TARGET.to_string());
        let text = lister
            .ask(&system, &corpus.join("\n"), 1000, 0.2)
            .await
            .ok_or("the list call failed")?;
        let json = first_json(&text).ok_or("the list reply held no JSON")?;
        json.get("macrotopics")
            .and_then(Value::as_array)
            .ok_or("the list reply held no `macrotopics`")?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    println!("macrotopics ({}): {}", list.len(), list.join(", "));

    let rendered = list
        .iter()
        .map(|m| format!("- {m}"))
        .collect::<Vec<_>>()
        .join("\n");
    let system = Arc::new(LABEL_SYSTEM.replace("<<list>>", &rendered));
    // The labeller is the cheap slot on purpose: this runs on every fact at
    // ingest, so a measurement made with a strong model would price a
    // pipeline nobody would deploy.
    let labeller = Arc::new(Caller::build(
        &{
            let mut m = o.clone();
            m.remove("model");
            m
        },
        "claude-haiku-4-5-20251001",
    )?);

    let batches: Vec<Vec<Fact>> = facts.chunks(LABEL_BATCH).map(<[Fact]>::to_vec).collect();
    let results = fan_out(batches, width(o), |batch| {
        let labeller = Arc::clone(&labeller);
        let system = Arc::clone(&system);
        let rendered: Vec<String> = batch.iter().map(Fact::render).collect();
        async move {
            let text = labeller
                .ask(&system, &rendered.join("\n"), 1500, 0.0)
                .await?;
            let json = first_json(&text)?;
            let mut out = Vec::new();
            for item in json.get("labels")?.as_array()? {
                out.push(Label {
                    fact: item.get("id")?.as_str()?.to_string(),
                    macrotopic: item
                        .get("macrotopic")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_lowercase(),
                    microtopic: item
                        .get("topic")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_lowercase(),
                });
            }
            Some(out)
        }
    })
    .await;

    let labels: Vec<Label> = results.into_iter().flatten().flatten().collect();
    append_jsonl(&out, &labels)?;

    let mut per_macro: BTreeMap<String, usize> = BTreeMap::new();
    let mut fine: HashSet<&str> = HashSet::new();
    for l in &labels {
        *per_macro.entry(l.macrotopic.clone()).or_default() += 1;
        if !l.microtopic.is_empty() {
            fine.insert(l.microtopic.as_str());
        }
    }
    println!("labelled     : {} of {} facts", labels.len(), facts.len());
    println!("distinct fine topics: {}", fine.len());
    for (m, n) in &per_macro {
        let name = if m.is_empty() { "(none)" } else { m.as_str() };
        println!("  {name:<18}: {n}");
    }
    println!("written      : {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// candidates — the five roads
// ---------------------------------------------------------------------------

/// Picks, for every fact, the facts a model will be asked about.
///
/// **This is the load-bearing step.** A nexus a model never sees a pair for
/// cannot be written, so the roads below set the ceiling on everything
/// measured downstream; the model can only lose recall from here, never add
/// it. Each road is filled independently and then merged, so a fact that
/// wins on two roads costs one slot, not two, and the roads that find
/// nothing quietly yield their budget to the ones that do.
/// An unordered pair, so the same two facts key the same slot whichever end
/// is asking.
fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Fills one road's quota from the cosine-sorted pool, skipping whatever an
/// earlier road already took. A road that finds nothing spends nothing: the
/// budget it does not use stays with the roads that do, because every road
/// draws from the same merged set.
fn fill(
    facts: &[Fact],
    scored: &[(usize, f32)],
    road: &str,
    quota: usize,
    taken: &mut HashMap<usize, Candidate>,
    keep: impl Fn(&Fact) -> bool,
) -> usize {
    let mut n = 0;
    for (j, cos) in scored {
        if n == quota {
            break;
        }
        if taken.contains_key(j) || !keep(&facts[*j]) {
            continue;
        }
        taken.insert(
            *j,
            Candidate {
                id: facts[*j].id.clone(),
                road: road.to_string(),
                cosine: *cos,
            },
        );
        n += 1;
    }
    n
}

async fn cmd_candidates(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    if facts.is_empty() {
        return Err("no facts in range".into());
    }
    let out = PathBuf::from(opt(o, "out")?);

    // Without labels the two topic roads simply find nothing, and the run
    // measures the other four. That is a different experiment, not a
    // broken one, so it is allowed — and it is said out loud.
    let labels: HashMap<String, Label> = match o.get("topics") {
        Some(path) => read_jsonl::<Label>(Path::new(path))?
            .into_iter()
            .map(|l| (l.fact.clone(), l))
            .collect(),
        None => {
            eprintln!("no --topics: the macrotopic and topic roads are OFF");
            HashMap::new()
        },
    };
    let label_of = |id: &str| labels.get(id).cloned().unwrap_or_default();

    // A deterministic shuffle for the chance road: no clock, no rng crate,
    // and a re-run offers the same pairs so a second weave is comparable.
    let mut counter: u64 = 0x9E37_79B9_7F4A_7C15;

    let mut batches = Vec::new();
    let mut per_road: BTreeMap<&str, usize> = BTreeMap::new();
    let mut offered: HashSet<(String, String)> = HashSet::new();

    for (i, fact) in facts.iter().enumerate() {
        let mut scored: Vec<(usize, f32)> = facts
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(j, other)| (j, cosine_similarity(&fact.vector, &other.vector)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));

        let mut taken: HashMap<usize, Candidate> = HashMap::new();
        let mine = label_of(&fact.id);

        let n_near = fill(&facts, &scored, "near", QUOTA_NEAR, &mut taken, |_| true);
        let n_day = fill(&facts, &scored, "day", QUOTA_DAY, &mut taken, |f| {
            day_gap(&f.day, &fact.day) <= DAY_WINDOW
        });
        let n_subj = fill(&facts, &scored, "subject", QUOTA_SUBJECT, &mut taken, |f| {
            f.subject == fact.subject
                || (f.subject_external.is_some() && f.subject_external == fact.subject_external)
        });
        // The coarse bucket WITHOUT the fine one: same area, different
        // thing inside it. Requiring the topics to differ is what keeps
        // this road from re-offering what `near` already found — two facts
        // with the same macrotopic AND the same topic are usually the same
        // sentence twice.
        let n_macro = fill(&facts, &scored, "macro", QUOTA_MACRO, &mut taken, |f| {
            let theirs = label_of(&f.id);
            !mine.macrotopic.is_empty()
                && theirs.macrotopic == mine.macrotopic
                && theirs.microtopic != mine.microtopic
        });
        let n_top = fill(&facts, &scored, "topic", QUOTA_TOPIC, &mut taken, |f| {
            let theirs = label_of(&f.id);
            !mine.microtopic.is_empty() && theirs.microtopic == mine.microtopic
        });

        // The chance road, drawn from the tail the four signals never reach.
        let mut n_chance = 0;
        for _ in 0..(QUOTA_CHANCE * 8) {
            if n_chance == QUOTA_CHANCE {
                break;
            }
            counter = counter
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(i as u64 + 1);
            let j = (counter >> 33) as usize % facts.len();
            if j == i || taken.contains_key(&j) {
                continue;
            }
            let cos = cosine_similarity(&fact.vector, &facts[j].vector);
            taken.insert(
                j,
                Candidate {
                    id: facts[j].id.clone(),
                    road: "chance".to_string(),
                    cosine: cos,
                },
            );
            n_chance += 1;
        }

        *per_road.entry("near").or_default() += n_near;
        *per_road.entry("day").or_default() += n_day;
        *per_road.entry("subject").or_default() += n_subj;
        *per_road.entry("macro").or_default() += n_macro;
        *per_road.entry("topic").or_default() += n_top;
        *per_road.entry("chance").or_default() += n_chance;

        // A pair is offered from ONE end only. Both ends see the same two
        // sentences, so a second offer buys a duplicate arc at full price —
        // and an arc is stored once anyway.
        let mut candidates: Vec<Candidate> = taken
            .into_values()
            .filter(|c| offered.insert(pair_key(&fact.id, &c.id)))
            .collect();
        candidates.sort_by(|a, b| b.cosine.total_cmp(&a.cosine));
        batches.push(Batch {
            fact: fact.id.clone(),
            candidates,
        });
    }

    let mut text = String::new();
    for b in &batches {
        text.push_str(&serde_json::to_string(b).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(&out, text).map_err(|e| format!("cannot write {}: {e}", out.display()))?;

    let pairs: usize = batches.iter().map(|b| b.candidates.len()).sum();
    println!("facts        : {}", facts.len());
    println!(
        "pairs offered: {pairs}  ({:.1} per fact)",
        pairs as f32 / facts.len() as f32
    );
    println!("by road, before the pair is deduplicated across its two ends:");
    for (road, n) in &per_road {
        println!("  {road:<8}: {n}");
    }
    println!("written      : {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// The model side
// ---------------------------------------------------------------------------

/// Tools the headless CLI is forbidden, so a call is a completion and not an
/// agent turn: the harness wants an answer, not a session that reads files.
const TOOLS_OFF: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "Task",
    "NotebookEdit",
    "TodoWrite",
];

/// Which door the model is reached through.
enum Via {
    /// `claude -p`, on the operator's subscription.
    Cli,
    /// The engine's own Anthropic backend, on whatever `ANTHROPIC_API_KEY` is.
    Api(Arc<dyn LlmBackend>),
}

/// One model, reachable either way, with the retry ladder both need. At any
/// useful concurrency a run meets the provider's rate limiter, and a sample
/// that silently loses its slow half reads like a result.
struct Caller {
    via: Via,
    model: String,
}

impl Caller {
    /// Builds the caller and says out loud which meter the run spins, because
    /// one env var can hold either kind of credential.
    fn build(o: &HashMap<String, String>, default_model: &str) -> Result<Self, String> {
        let model = o
            .get("model")
            .map_or(default_model, String::as_str)
            .to_string();
        match o.get("via").map_or("cli", String::as_str) {
            "cli" => {
                eprintln!("model  : {model} via `claude -p` (subscription, flat rate)");
                Ok(Self {
                    via: Via::Cli,
                    model,
                })
            },
            "api" => {
                let key = std::env::var(ANTHROPIC_KEY_ENV)
                    .map_err(|_| format!("{ANTHROPIC_KEY_ENV} is not set"))?;
                let metered = key.trim().starts_with("sk-ant-api");
                eprintln!(
                    "model  : {model} via the API ({})",
                    if metered {
                        "CONSOLE KEY — BILLED PER TOKEN"
                    } else {
                        "subscription token"
                    }
                );
                let backend = AnthropicBackend::new(
                    AnthropicApiKey::new(key)
                        .map_err(|e| format!("bad {ANTHROPIC_KEY_ENV}: {e}"))?,
                    model.clone(),
                    ANTHROPIC_KEY_ENV,
                )
                .map_err(|e| format!("cannot build the Anthropic backend: {e}"))?;
                Ok(Self {
                    via: Via::Api(Arc::new(backend)),
                    model,
                })
            },
            other => Err(format!("unknown `--via {other}` — expected `cli` or `api`")),
        }
    }

    /// One completion, retrying what is worth retrying. A credential or a
    /// malformed request fails at once rather than burning four more attempts.
    async fn ask(
        &self,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        temperature: f32,
    ) -> Option<String> {
        let mut attempt = 1;
        loop {
            let outcome = match &self.via {
                Via::Cli => self.ask_cli(system, prompt).await,
                Via::Api(backend) => backend
                    .complete(CompletionRequest {
                        prompt: prompt.to_string(),
                        system: Some(system.to_string()),
                        max_tokens: Some(max_tokens),
                        temperature: Some(temperature),
                        stop: Vec::new(),
                        images: Vec::new(),
                        truncation_expected: false,
                        cache_system: true,
                    })
                    .await
                    .map(|r| r.text)
                    .map_err(|e| {
                        let fatal = matches!(e, LlmError::Invalid(_) | LlmError::Auth(_));
                        (format!("{e}"), fatal)
                    }),
            };
            match outcome {
                Ok(text) => return Some(text),
                Err((why, true)) => {
                    eprintln!("  giving up: {why}");
                    return None;
                },
                Err((why, false)) if attempt >= MAX_ATTEMPTS => {
                    eprintln!("  giving up after {attempt}: {why}");
                    return None;
                },
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        BACKOFF_BASE_MS << (attempt - 1),
                    ))
                    .await;
                    attempt += 1;
                },
            }
        }
    }

    /// The prompt rides stdin: `--disallowed-tools` is variadic and would
    /// swallow a positional argument, and a batch of twenty facts outgrows a
    /// comfortable argv anyway.
    async fn ask_cli(&self, system: &str, prompt: &str) -> Result<String, (String, bool)> {
        use tokio::io::AsyncWriteExt as _;

        let mut child = tokio::process::Command::new("claude")
            .arg("-p")
            .arg("--model")
            .arg(&self.model)
            .arg("--output-format")
            .arg("text")
            .arg("--system-prompt")
            .arg(system)
            .arg("--strict-mcp-config")
            .arg("--disallowed-tools")
            .args(TOOLS_OFF)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| (format!("cannot run `claude`: {e}"), true))?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            drop(stdin);
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| (e.to_string(), false))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err((
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
                false,
            ))
        }
    }
}

/// Pulls the first JSON object out of a reply that may be fenced or prefaced.
fn first_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {},
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=i]).ok();
                }
            },
            _ => {},
        }
    }
    None
}

/// The rules both model passes share: what the eight types mean, and that
/// most pairs hold nothing.
fn types_block() -> String {
    use std::fmt::Write as _;
    let mut s = String::from("The eight nexus types, and nothing outside this list:\n\n");
    for (name, gloss) in NEXUS_TYPES {
        let _ = writeln!(s, "- `{name}` — {gloss}");
    }
    s
}

// ---------------------------------------------------------------------------
// weave
// ---------------------------------------------------------------------------

const WEAVE_SYSTEM: &str = "\
You link facts in a memory. You are given ONE fact and a list of OTHER facts,
and for each pair you decide whether a real relation holds between them.

MOST PAIRS HOLD NOTHING. They were offered because a search found them near
each other, and nearness is not a relation: two lab results from the same
week, two errands on the same day, two facts about the same person are
merely ABOUT the same thing. Returning an empty list is the common and
correct answer, and a list of ten is almost certainly wrong.

A relation holds only when knowing one fact changes what the other MEANS —
when a reader who has both understands something neither says alone.

<<types>>

FOUR WAYS TO GET THIS WRONG. Each one is a refusal, not a judgement call.

1. INVENTING THE REASON. If neither fact states why, there is no cause. Do
   not supply a plausible motive, a purpose, or an intention that only makes
   sense once you have assumed it. `Being a mother` does not cause a meal
   plan; liking a tool does not cause a billing choice.
2. RESTATING. If your sentence is the two facts joined by a comma, or one
   fact with a synonym swapped in, there is no relation — you have written a
   summary. And if one fact ALREADY CONTAINS the whole relation on its own,
   the other fact adds nothing and the pair holds nothing.
3. DATES ARE NOT SUCCESSION. Two things happening in an order is chronology.
   `successione` needs the first to lead into the second — a course of
   events, not two entries in a diary.
4. A DUPLICATE IS NOT A CONTRAST. Two records of the same thing that
   disagree — one name spelt two ways, one field with two values — are a
   contradiction for somebody to resolve, not a `contrasto`. A `contrasto`
   needs two DIFFERENT things that a reader would compare.

For each nexus you do find, write ONE sentence that states the relation
itself, in the language the facts are written in. The sentence is the point:
it is what a future reader searches for and finds. So write the relation,
not the two facts joined by a comma.

Write what is true HERE, of these people and these things. Whoever reads
this memory already knows how the world works and can look anything up; what
they cannot look up is the particular. So never write the general law — write
what it did in this case.

- good: \"non puo' aiutare a spostare la lavatrice perche' non solleva pesi\"
- bad : \"la disidratazione fa salire la creatinina\" (a textbook line)
- bad : \"beve poco e la creatinina e' 2,86\" (two facts, no relation)

Answer with JSON and nothing else:

{\"nexuses\": [{\"to\": \"<candidate id>\", \"type\": \"<one of the eight>\",
  \"direction\": \"from-to\" | \"to-from\", \"sentence\": \"<the relation>\"}]}

`direction` says which way the type runs: `from-to` when THE FACT is the
cause / the constraint / the instance, `to-from` when the candidate is.";

async fn cmd_weave(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let by_id: HashMap<&str, &Fact> = facts.iter().map(|f| (f.id.as_str(), f)).collect();

    let batches: Vec<Batch> = read_jsonl(Path::new(opt(o, "in")?))?;
    let out = PathBuf::from(opt(o, "out")?);
    let caller = Arc::new(Caller::build(o, "claude-haiku-4-5-20251001")?);
    let system = WEAVE_SYSTEM.replace("<<types>>", &types_block());

    let done = already_done(&out, "batch");
    let todo: Vec<&Batch> = batches.iter().filter(|b| !done.contains(&b.fact)).collect();
    eprintln!("batches: {} ({} already done)", todo.len(), done.len());

    let results = fan_out(todo, width(o), |batch| {
        let caller = Arc::clone(&caller);
        let system = system.clone();
        let fact = (*by_id
            .get(batch.fact.as_str())
            .expect("batch fact is in range"))
        .clone();
        let roads: HashMap<String, String> = batch
            .candidates
            .iter()
            .map(|c| (c.id.clone(), c.road.clone()))
            .collect();
        let rendered: Vec<String> = batch
            .candidates
            .iter()
            .filter_map(|c| by_id.get(c.id.as_str()).map(|f| f.render()))
            .collect();
        async move {
            let prompt = format!(
                "THE FACT\n\n{}\n\nOTHER FACTS\n\n{}\n",
                fact.render(),
                rendered.join("\n")
            );
            let text = caller.ask(&system, &prompt, 2048, 0.1).await?;
            let json = first_json(&text)?;
            let arr = json.get("nexuses")?.as_array()?.clone();
            let mut out = Vec::new();
            for item in arr {
                let to = item.get("to")?.as_str()?.to_string();
                let kind = item.get("type")?.as_str()?.to_string();
                let sentence = item.get("sentence")?.as_str()?.to_string();
                let flip = item.get("direction").and_then(Value::as_str) == Some("to-from");
                let (from, to) = if flip {
                    (to.clone(), fact.id.clone())
                } else {
                    (fact.id.clone(), to.clone())
                };
                let road = roads
                    .get(&to)
                    .or_else(|| roads.get(&from))
                    .cloned()
                    .unwrap_or_default();
                out.push(Nexus {
                    batch: fact.id.clone(),
                    from,
                    to,
                    kind,
                    sentence,
                    road,
                });
            }
            Some(out)
        }
    })
    .await;

    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let woven: Vec<Nexus> = results
        .into_iter()
        .flatten()
        .flatten()
        .filter(|n| {
            let (a, b) = pair_key(&n.from, &n.to);
            seen.insert((a, b, n.kind.clone()))
        })
        .collect();
    append_jsonl(&out, &woven)?;

    let mut per_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_road: BTreeMap<String, usize> = BTreeMap::new();
    for n in &woven {
        *per_type.entry(n.kind.clone()).or_default() += 1;
        *per_road.entry(n.road.clone()).or_default() += 1;
    }
    println!("nexuses woven: {}", woven.len());
    println!("by type:");
    for (k, v) in &per_type {
        println!("  {k:<14}: {v}");
    }
    println!("by road:");
    for (k, v) in &per_road {
        println!("  {k:<14}: {v}");
    }
    println!("written      : {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// judge
// ---------------------------------------------------------------------------

const JUDGE_SYSTEM: &str = "\
You are re-reading ONE proposed link between two facts in a memory, and your
job is to REFUSE it unless it clearly holds. Somebody else proposed it; you
owe them nothing.

<<types>>

Refuse when:
- the two facts are merely ABOUT the same person, day, or topic;
- the sentence restates the two facts instead of stating a relation;
- the relation is real but the TYPE is wrong;
- the direction is backwards;
- the sentence asserts something neither fact supports — an invented cause,
  an invented purpose, a plausible story.

Accept only when a reader who knows both facts would recognise the sentence
as true, and as saying something neither fact says alone.

Answer with JSON and nothing else:

{\"holds\": true|false, \"why\": \"<one short clause>\"}";

async fn cmd_judge(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let by_id: HashMap<&str, &Fact> = facts.iter().map(|f| (f.id.as_str(), f)).collect();

    let nexuses: Vec<Nexus> = read_jsonl(Path::new(opt(o, "in")?))?;
    let out = PathBuf::from(opt(o, "out")?);
    let caller = Arc::new(Caller::build(o, "claude-opus-5")?);
    let system = JUDGE_SYSTEM.replace("<<types>>", &types_block());
    eprintln!("nexuses to judge: {}", nexuses.len());

    let refs: Vec<&Nexus> = nexuses.iter().collect();
    let results = fan_out(refs, width(o), |n| {
        let caller = Arc::clone(&caller);
        let system = system.clone();
        let n = (*n).clone();
        let from = by_id.get(n.from.as_str()).map(|f| f.render());
        let to = by_id.get(n.to.as_str()).map(|f| f.render());
        async move {
            let (from, to) = (from?, to?);
            let prompt = format!(
                "FACT A\n{from}\n\nFACT B\n{to}\n\nPROPOSED\ntype: {}\ndirection: A -> B\nsentence: {}\n",
                n.kind, n.sentence
            );
            let text = caller.ask(&system, &prompt, 300, 0.0).await?;
            let json = first_json(&text)?;
            Some(Verdict {
                from: n.from,
                to: n.to,
                kind: n.kind,
                holds: json.get("holds")?.as_bool()?,
                why: json.get("why").and_then(Value::as_str).unwrap_or_default().to_string(),
                road: n.road,
            })
        }
    })
    .await;

    let verdicts: Vec<Verdict> = results.into_iter().flatten().collect();
    append_jsonl(&out, &verdicts)?;

    let held = verdicts.iter().filter(|v| v.holds).count();
    let mut per_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut per_road: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for v in &verdicts {
        let e = per_type.entry(v.kind.clone()).or_default();
        e.1 += 1;
        e.0 += usize::from(v.holds);
        let e = per_road.entry(v.road.clone()).or_default();
        e.1 += 1;
        e.0 += usize::from(v.holds);
    }
    println!(
        "judged {} — {held} hold ({:.0}%)",
        verdicts.len(),
        100.0 * held as f32 / verdicts.len().max(1) as f32
    );
    println!("by type (held/judged):");
    for (k, (h, n)) in &per_type {
        println!(
            "  {k:<14}: {h}/{n}  ({:.0}%)",
            100.0 * *h as f32 / *n as f32
        );
    }
    println!("by road (held/judged):");
    for (k, (h, n)) in &per_road {
        println!(
            "  {k:<14}: {h}/{n}  ({:.0}%)",
            100.0 * *h as f32 / *n as f32
        );
    }
    println!("written      : {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// nodes
// ---------------------------------------------------------------------------

const NODES_SYSTEM: &str = "\
You write the sentence a cluster of related facts is ABOUT.

You are given several facts and the relations somebody has already found
between them. Write ONE sentence, in the language of the facts, that states
what the whole cluster amounts to — the rule, the tension, the causal chain
that the facts are separate pieces of.

Nobody uttered this sentence. That is the point: it says the thing the facts
only imply, and it is written so that a future fact about the same situation
lands near it even when it resembles none of the facts here.

THE ONE TEST IT MUST PASS: the sentence must be true HERE AND NOWHERE ELSE —
of this machine, this person, this household. A sentence that could be
looked up is not a memory. Whoever reads this memory already knows how the
world works, and knows it better than you do; what they cannot know is the
particular.

- good: \"da' problemi solo con la carta spessa\" (this machine, learned here)
- bad : \"non bere abbastanza fa salire la creatinina\" (a textbook line)
- bad : \"tre volte la macchina si e' bloccata\" (a summary, not a relation)

When the only sentence you can write for a cluster is a general truth, say
so and write nothing: an empty answer is correct and common.

Answer with JSON and nothing else:

{\"sentence\": \"<the relation, or an empty string>\", \"covers\": [\"<fact id>\", ...]}

Drop from `covers` any fact the sentence does not actually speak for.";

async fn cmd_nodes(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let by_id: HashMap<&str, &Fact> = facts.iter().map(|f| (f.id.as_str(), f)).collect();

    // Clusters are the connected components of the graph the judge kept:
    // a node speaks for a group of facts that hold together, and "holds
    // together" is exactly what an edge asserts.
    let verdicts: Vec<Verdict> = read_jsonl(Path::new(opt(o, "in")?))?;
    let kept: Vec<&Verdict> = verdicts.iter().filter(|v| v.holds).collect();
    let clusters = components(&kept);
    let big: Vec<&Vec<String>> = clusters.iter().filter(|c| c.len() >= 2).collect();
    eprintln!("clusters: {} (of {} components)", big.len(), clusters.len());

    let out = PathBuf::from(opt(o, "out")?);
    let caller = Arc::new(Caller::build(o, "claude-opus-5")?);

    let results = fan_out(big, width(o), |cluster| {
        let caller = Arc::clone(&caller);
        let rendered: Vec<String> = cluster
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|f| f.render()))
            .collect();
        let relations: Vec<String> = kept
            .iter()
            .filter(|v| cluster.contains(&v.from) && cluster.contains(&v.to))
            .map(|v| format!("- ({}) {}", v.kind, v.why))
            .collect();
        async move {
            let prompt = format!(
                "FACTS\n\n{}\n\nRELATIONS ALREADY FOUND\n\n{}\n",
                rendered.join("\n"),
                relations.join("\n")
            );
            let text = caller.ask(NODES_SYSTEM, &prompt, 500, 0.2).await?;
            let json = first_json(&text)?;
            let sentence = json.get("sentence")?.as_str()?.trim().to_string();
            if sentence.is_empty() {
                return None;
            }
            Some(Node {
                sentence,
                covers: json
                    .get("covers")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                vector: Vec::new(),
            })
        }
    })
    .await;

    let mut nodes: Vec<Node> = results.into_iter().flatten().collect();

    // A node with no vector cannot attract, which is the whole of its job.
    let embedder = load_embedder(o)?;
    for node in &mut nodes {
        node.vector = embedder
            .embed(&node.sentence)
            .await
            .map_err(|e| format!("cannot embed a node: {e}"))?;
    }

    append_jsonl(&out, &nodes)?;
    println!("nodes written: {} -> {}", nodes.len(), out.display());
    for n in nodes.iter().take(10) {
        println!("  [{} facts] {}", n.covers.len(), n.sentence);
    }
    Ok(())
}

/// Walks the parent chain to the representative of `id`.
fn root(parent: &HashMap<String, String>, id: &str) -> String {
    let mut cur = id.to_string();
    while let Some(next) = parent.get(&cur) {
        if next == &cur {
            break;
        }
        cur = next.clone();
    }
    cur
}

/// Connected components over the kept edges, as sorted id lists, largest
/// first. An edge asserts that two facts hold together, which is exactly the
/// membership a node needs to speak for a group.
fn components(edges: &[&Verdict]) -> Vec<Vec<String>> {
    let mut parent: HashMap<String, String> = HashMap::new();
    for e in edges {
        parent
            .entry(e.from.clone())
            .or_insert_with(|| e.from.clone());
        parent.entry(e.to.clone()).or_insert_with(|| e.to.clone());
        let (a, b) = (root(&parent, &e.from), root(&parent, &e.to));
        if a != b {
            parent.insert(a, b);
        }
    }
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    let ids: Vec<String> = parent.keys().cloned().collect();
    for id in ids {
        let r = root(&parent, &id);
        groups.entry(r).or_default().push(id);
    }
    let mut out: Vec<Vec<String>> = groups.into_values().collect();
    for g in &mut out {
        g.sort();
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.len()));
    out
}

// ---------------------------------------------------------------------------
// ask
// ---------------------------------------------------------------------------

/// How many facts each way returns, so the two columns are comparable.
const ASK_DEPTH: usize = 10;

/// Answers one query twice: flat over facts, then over facts and nodes with
/// the nexuses walked. Prints both columns side by side, because the
/// question is never "does it find something" but "does it find the thing
/// the other one missed".
async fn cmd_ask(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let by_id: HashMap<&str, &Fact> = facts.iter().map(|f| (f.id.as_str(), f)).collect();
    let query = opt(o, "query")?;

    let embedder = load_embedder(o)?;
    let qv = embedder
        .embed(query)
        .await
        .map_err(|e| format!("cannot embed the query: {e}"))?;

    let mut flat: Vec<(&Fact, f32)> = facts
        .iter()
        .map(|f| (f, cosine_similarity(&qv, &f.vector)))
        .collect();
    flat.sort_by(|a, b| b.1.total_cmp(&a.1));

    println!("== flat over facts ==");
    for (f, s) in flat.iter().take(ASK_DEPTH) {
        println!("  {s:.3}  {}", short(&f.text));
    }

    let Some(nodes_path) = o.get("nodes") else {
        return Ok(());
    };
    let nodes: Vec<Node> = read_jsonl(Path::new(nodes_path))?;
    let verdicts: Vec<Verdict> = match o.get("nexuses") {
        Some(p) => read_jsonl(Path::new(p))?,
        None => Vec::new(),
    };

    let mut ranked_nodes: Vec<(&Node, f32)> = nodes
        .iter()
        .filter(|n| !n.vector.is_empty())
        .map(|n| (n, cosine_similarity(&qv, &n.vector)))
        .collect();
    ranked_nodes.sort_by(|a, b| b.1.total_cmp(&a.1));

    println!("\n== nodes ==");
    for (n, s) in ranked_nodes.iter().take(3) {
        println!("  {s:.3}  {}", short(&n.sentence));
    }

    // The graph column: the facts the best nodes speak for, then one step
    // along every kept nexus out of them. A fact reached this way is scored
    // by the node that reached it, never by its own likeness to the query —
    // that is the point of a walk.
    let mut reached: Vec<(&Fact, f32, &'static str)> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (node, score) in ranked_nodes.iter().take(3) {
        for id in &node.covers {
            if let Some(f) = by_id.get(id.as_str())
                && seen.insert(f.id.as_str())
            {
                reached.push((f, *score, "node"));
            }
        }
    }
    let frontier: Vec<String> = reached.iter().map(|(f, ..)| f.id.clone()).collect();
    for v in verdicts.iter().filter(|v| v.holds) {
        for (a, b) in [(&v.from, &v.to), (&v.to, &v.from)] {
            if frontier.contains(a)
                && let Some(f) = by_id.get(b.as_str())
                && seen.insert(f.id.as_str())
            {
                reached.push((f, 0.0, "walk"));
            }
        }
    }
    reached.sort_by(|a, b| b.1.total_cmp(&a.1));

    println!("\n== nodes + walk ==");
    for (f, s, how) in reached.iter().take(ASK_DEPTH) {
        let flat_rank = flat
            .iter()
            .position(|(g, _)| g.id == f.id)
            .map_or(999, |p| p + 1);
        let mark = if flat_rank > ASK_DEPTH {
            " <- flat misses this"
        } else {
            ""
        };
        println!("  {s:.3} {how:<5} #{flat_rank:<4} {}{mark}", short(&f.text));
    }
    Ok(())
}

/// Facts the flat pass keeps, matching the engine's own flat slot
/// (`IngestPolicy::recall_top_k`), so the two columns are read at the depth
/// the product actually serves.
const EVAL_FLAT_DEPTH: usize = 10;

/// Extra facts the macrotopic hop may add on top of the flat pass. Same
/// order of magnitude as the flat slot: a road that had to double the block
/// to find anything would be paying for its hits with the reader's attention.
const EVAL_HOP_DEPTH: usize = 10;

/// Overridden by `--hop`, so the same run can ask what the road costs at half
/// the budget without a rebuild.

/// Flat hits whose labels seed the hop. Only what a reader would actually
/// have looked at can say what the turn is ABOUT.
const EVAL_SEED_HITS: usize = 3;

/// One gold entry, in the shape `recall eval` already reads so the same file
/// scores both engines.
#[derive(Deserialize)]
struct GoldQuery {
    #[serde(default)]
    id: Option<String>,
    query: String,
    expect: Vec<String>,
}

#[derive(Deserialize)]
struct GoldSet {
    queries: Vec<GoldQuery>,
}

/// Scores a gold set two ways and prints the difference.
///
/// **The question is not whether the macrotopic finds something — it is
/// whether it finds what the vector could not.** So the hop is scored as an
/// ADDITION to the flat pass, never as a replacement: the flat block is kept
/// whole and the road is charged only with what it adds beyond it.
///
/// The hop is the only shape available at query time. A question has no
/// macrotopic of its own — nobody labelled it — so the road reads the labels
/// off the facts the vector already found, and pulls in their neighbours that
/// share a macrotopic and differ in microtopic. That last clause is the road:
/// same area, different thing inside it, which is exactly the pair the vector
/// puts on top of each other and cannot separate.
async fn cmd_eval(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let labels: HashMap<String, Label> = read_jsonl::<Label>(Path::new(opt(o, "topics")?))?
        .into_iter()
        .map(|l| (l.fact.clone(), l))
        .collect();
    let gold: GoldSet = serde_yaml::from_str(
        &fs::read_to_string(opt(o, "gold")?).map_err(|e| format!("cannot read the gold: {e}"))?,
    )
    .map_err(|e| format!("cannot parse the gold: {e}"))?;

    let embedder = load_embedder(o)?;
    // The control the road has to beat. The hop adds facts, so some of what
    // it finds would have arrived from depth alone — and a road that buys
    // only what a bigger `top_k` buys is a knob, not a road.
    let hop_depth = o
        .get("hop")
        .and_then(|h| h.parse().ok())
        .unwrap_or(EVAL_HOP_DEPTH);
    let deep_depth = EVAL_FLAT_DEPTH + hop_depth;
    let (mut flat_hit, mut hop_hit, mut deep_hit, mut total) = (0usize, 0usize, 0usize, 0usize);

    for q in &gold.queries {
        let qv = embedder
            .embed(&q.query)
            .await
            .map_err(|e| format!("cannot embed a query: {e}"))?;
        let mut ranked: Vec<(&Fact, f32)> = facts
            .iter()
            .map(|f| (f, cosine_similarity(&qv, &f.vector)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

        let flat: Vec<&Fact> = ranked
            .iter()
            .take(EVAL_FLAT_DEPTH)
            .map(|(f, _)| *f)
            .collect();
        let seeds: HashSet<String> = flat
            .iter()
            .take(EVAL_SEED_HITS)
            .filter_map(|f| labels.get(&f.id))
            .map(|l| l.macrotopic.clone())
            .filter(|m| !m.is_empty())
            .collect();
        let seen: HashSet<&str> = flat.iter().map(|f| f.id.as_str()).collect();
        // Microtopics the block already speaks for. The road exists to bring
        // back what the block does NOT already say, so a fact repeating one of
        // these is not a gain however near it sits.
        let mut spoken: HashSet<String> = flat
            .iter()
            .filter_map(|f| labels.get(&f.id))
            .map(|l| l.microtopic.clone())
            .collect();
        // **One fact per microtopic, and never a microtopic twice.** Taking
        // the ten NEAREST of the area would reproduce the exact failure this
        // road exists to fix: four sentences of the shape "X takes Y at hour
        // Z" sit on top of each other in vector space, so the nearest of them
        // is also the least informative — the block already holds its twin.
        // Cosine order survives only as a tie-break WITHIN a microtopic: of
        // two facts saying different things, the nearer one represents its own
        // better.
        let hop: Vec<&Fact> = ranked
            .iter()
            .map(|(f, _)| *f)
            .filter(|f| !seen.contains(f.id.as_str()))
            .filter(|f| {
                labels.get(&f.id).is_some_and(|l| {
                    seeds.contains(&l.macrotopic)
                        && !l.microtopic.is_empty()
                        && spoken.insert(l.microtopic.clone())
                })
            })
            .take(hop_depth)
            .collect();

        let deep: Vec<&Fact> = ranked.iter().take(deep_depth).map(|(f, _)| *f).collect();
        let covers = |pool: &[&Fact], needle: &str| {
            let needle = needle.to_lowercase();
            pool.iter().any(|f| f.text.to_lowercase().contains(&needle))
        };
        let together: Vec<&Fact> = flat.iter().chain(hop.iter()).copied().collect();
        let (mut f_n, mut h_n, mut d_n) = (0usize, 0usize, 0usize);
        let mut gained: Vec<&str> = Vec::new();
        for e in &q.expect {
            if covers(&flat, e) {
                f_n += 1;
            }
            if covers(&together, e) {
                h_n += 1;
                if !covers(&flat, e) {
                    gained.push(if covers(&deep, e) {
                        "(anche in profondità)"
                    } else {
                        e.as_str()
                    });
                }
            }
            if covers(&deep, e) {
                d_n += 1;
            }
        }
        flat_hit += f_n;
        hop_hit += h_n;
        deep_hit += d_n;
        total += q.expect.len();
        let label = q.id.as_deref().unwrap_or(&q.query);
        let mark = if gained.is_empty() {
            String::new()
        } else {
            format!("  +{}", gained.join(", "))
        };
        println!(
            "  {label:<34} piatto {f_n}/{n} · piatto profondo {d_n}/{n} · col macrotopic {h_n}/{n} (+{hop}){mark}",
            n = q.expect.len(),
            hop = hop.len()
        );
    }
    let pct = |x: usize| 100.0 * x as f32 / total.max(1) as f32;
    println!(
        "\ncopertura su {total} attese:\n  piatto ({EVAL_FLAT_DEPTH} fatti)         {:.0}%  ({flat_hit})\n  \
         piatto profondo ({deep_depth} fatti) {:.0}%  ({deep_hit})   <- il controllo\n  \
         col macrotopic          {:.0}%  ({hop_hit})",
        pct(flat_hit),
        pct(deep_hit),
        pct(hop_hit)
    );
    Ok(())
}

/// Words offered to a labeller as "these already exist". Twenty short words
/// is a couple of hundred characters of prompt — the price of not coining a
/// synonym of something already in the vocabulary.
const VOCAB_SHORTLIST: usize = 20;

/// Asks whether a vocabulary of topic words can be kept free of duplicates by
/// showing a labeller the words already near the fact it is about to label.
///
/// Two things have to hold and neither is obvious. **A synonym must sit
/// nearer its twin than an unrelated word of the same area does** — otherwise
/// a shortlist is noise. And **the word a fact actually deserves must be
/// reachable from the fact's own vector** — otherwise the shortlist never
/// contains the right answer and the labeller coins a new word every time,
/// which is the state this design exists to leave.
async fn cmd_vocab(o: &HashMap<String, String>) -> Result<(), String> {
    let db = PathBuf::from(opt(o, "db")?);
    let facts = load_facts(&db, o.get("before").map(String::as_str)).await?;
    let labels: HashMap<String, Label> = read_jsonl::<Label>(Path::new(opt(o, "topics")?))?
        .into_iter()
        .map(|l| (l.fact.clone(), l))
        .collect();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for l in labels.values() {
        for w in [&l.macrotopic, &l.microtopic] {
            if !w.is_empty() {
                *counts.entry(w.clone()).or_default() += 1;
            }
        }
    }
    let words: Vec<String> = counts.keys().cloned().collect();
    println!("parole distinte nel vocabolario: {}", words.len());

    let embedder = load_embedder(o)?;
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(words.len());
    for w in &words {
        vectors.push(
            embedder
                .embed(w)
                .await
                .map_err(|e| format!("cannot embed `{w}`: {e}"))?,
        );
    }

    // 1. Do known synonyms come out on top of each other?
    println!("\n== i vicini di una parola, per vettore ==");
    for probe in [
        "nutrizione",
        "pressione",
        "renale",
        "finanziamento",
        "integratori",
    ] {
        let Some(i) = words.iter().position(|w| w == probe) else {
            continue;
        };
        let mut near: Vec<(&str, f32)> = words
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(j, w)| (w.as_str(), cosine_similarity(&vectors[i], &vectors[j])))
            .collect();
        near.sort_by(|a, b| b.1.total_cmp(&a.1));
        let shown: Vec<String> = near
            .iter()
            .take(5)
            .map(|(w, s)| format!("{w} {s:.2}"))
            .collect();
        println!("  {probe:<16} -> {}", shown.join(" · "));
    }

    // 2. Would the shortlist have contained the word the fact actually got?
    let mut reachable = 0usize;
    let mut judged = 0usize;
    for f in &facts {
        let Some(l) = labels.get(&f.id) else { continue };
        if l.microtopic.is_empty() {
            continue;
        }
        judged += 1;
        let mut near: Vec<(&str, f32)> = words
            .iter()
            .enumerate()
            .map(|(j, w)| (w.as_str(), cosine_similarity(&f.vector, &vectors[j])))
            .collect();
        near.sort_by(|a, b| b.1.total_cmp(&a.1));
        if near
            .iter()
            .take(VOCAB_SHORTLIST)
            .any(|(w, _)| *w == l.microtopic)
        {
            reachable += 1;
        }
    }
    println!(
        "\n== la parola scelta era fra le {VOCAB_SHORTLIST} più vicine al fatto? ==\n  \
         {reachable} su {judged}  ({:.0}%)   [lista corta calcolata sui VETTORI DELLE PAROLE]",
        100.0 * reachable as f32 / judged.max(1) as f32
    );

    // The cheap shortlist: the words already written on the facts recall
    // found. Nothing new is embedded — the block is already the material
    // nearest the turn, so the vocabulary on it is the vocabulary near the
    // turn. Whether that is as good as asking the words themselves is the
    // difference between a design that works and one that looks like it does.
    for depth in [10usize, 20] {
        let mut hit = 0usize;
        let mut seen = 0usize;
        for f in &facts {
            let Some(l) = labels.get(&f.id) else { continue };
            if l.microtopic.is_empty() {
                continue;
            }
            seen += 1;
            let mut near: Vec<(&Fact, f32)> = facts
                .iter()
                .filter(|g| g.id != f.id)
                .map(|g| (g, cosine_similarity(&f.vector, &g.vector)))
                .collect();
            near.sort_by(|a, b| b.1.total_cmp(&a.1));
            let offered: HashSet<&str> = near
                .iter()
                .take(depth)
                .filter_map(|(g, _)| labels.get(&g.id))
                .flat_map(|gl| [gl.macrotopic.as_str(), gl.microtopic.as_str()])
                .filter(|w| !w.is_empty())
                .collect();
            if offered.contains(l.microtopic.as_str()) {
                hit += 1;
            }
        }
        println!(
            "  {hit} su {seen}  ({:.0}%)   [lista corta presa dai {depth} FATTI più vicini — gratis]",
            100.0 * hit as f32 / seen.max(1) as f32
        );
    }
    Ok(())
}

fn short(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 110 {
        one
    } else {
        format!("{}…", one.chars().take(109).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

fn load_embedder(o: &HashMap<String, String>) -> Result<LocalEmbedder, String> {
    let dir = o.get("models").cloned().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.cache/mwe-mcp/{DEFAULT_MODEL_DIR}")
    });
    LocalEmbedder::load(Path::new(&dir), Device::Cpu, "bge-m3")
        .map_err(|e| format!("cannot load the embedder from {dir}: {e}"))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?,
        );
    }
    Ok(out)
}

fn append_jsonl<T: Serialize>(path: &Path, items: &[T]) -> Result<(), String> {
    let mut text = fs::read_to_string(path).unwrap_or_default();
    for item in items {
        text.push_str(&serde_json::to_string(item).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Ids already present in an output file, so a killed run resumes instead of
/// re-buying answers it has.
fn already_done(path: &Path, key: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let Some(id) = v.get(key).and_then(Value::as_str)
            {
                seen.insert(id.to_string());
            }
        }
    }
    seen
}

/// Runs `f` over every item with at most `width` calls in flight, refilling
/// as each finishes. A task that panics or a call that gives up drops its
/// item rather than the run.
async fn fan_out<I, F, Fut, T>(items: Vec<I>, width: usize, f: F) -> Vec<T>
where
    I: Send + Sync,
    F: Fn(&I) -> Fut + Send,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let total = items.len();
    let mut out = Vec::with_capacity(total);
    let mut set = tokio::task::JoinSet::new();
    let mut pending = items.iter();
    for item in pending.by_ref().take(width) {
        set.spawn(f(item));
    }
    while let Some(joined) = set.join_next().await {
        if let Ok(value) = joined {
            out.push(value);
        }
        let seen = out.len();
        if seen % 10 == 0 || seen == total {
            eprintln!("  {seen}/{total}");
        }
        if let Some(item) = pending.next() {
            set.spawn(f(item));
        }
    }
    out
}
