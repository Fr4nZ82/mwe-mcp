// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planning card 65 — does flat recall get better when it knows WHO the turn
//! is about?
//!
//! Two defects, measured side by side against the ranking production actually
//! serves (every variant is built on `wiki_search_unrecorded`, so the ACL
//! filter, the closed-window down-rank and the ordering are the real ones and
//! not a second implementation that could drift):
//!
//! - **A — the asker is not in the query.** "what music do *I* and X like"
//!   embeds nothing of the asker: a pronoun denotes nobody until something
//!   resolves it, and the engine has known the speaker since before it read
//!   the turn. Variant `rewrite` substitutes the first person and re-embeds.
//! - **B — covering two subjects earns nothing.** Scoring is cosine alone, so
//!   a fact naming both people of a two-person question ranks like one naming
//!   neither. Variant `cover:<w>` adds a bonus per subject covered beyond the
//!   first — a ranking signal, never a filter, exactly like the closed-window
//!   down-rank it sits beside.
//!
//! **Both numbers are reported for every variant, and the second is the one
//! that decides**: whether the answer climbs, AND whether facts about the
//! asker that have nothing to do with the question climb with it. A variant
//! that wins the first and loses the second has moved the crowding, not
//! removed it — the same failure the navigator's principal seed had before its
//! weight dropped to the ordinary rung (card 63).
//!
//! Read-only: recall counters are not bumped. Local embedder, no API spend.
//!
//! ```text
//! cargo run -p mwe-core --example subject_signal --features local-embedder --release -- \
//!     --workdir <copy> --turns <file> --top-k 5 --weights 0.02,0.05,0.10
//! ```
//!
//! `--turns` is TSV, `#` comments and blank lines ignored:
//!
//! ```text
//! <sender-id> <TAB> the turn, verbatim [ <TAB> the fact id that answers it ]
//! ```
//!
//! The third column is optional and is what makes the rank column readable;
//! without it a turn still contributes its asker-crowding number.

#![allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    reason = "measurement harness: the report code is written for a human \
              reading numbers once, not for reuse"
)]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mwe_core::embedder::Embedder;
use mwe_core::fact_index::FactIndexRow;
use mwe_core::recall::{RecallHit, SUBJECT_COVERAGE_UPLIFT, SenderContext, wiki_search_unrecorded};
use mwe_core::types::Principal;
use mwe_core::{db, enrollment, fact_index};

/// Ask for the whole visible corpus, ranked — the variants re-order it
/// themselves, so a top-k here would decide the answer before they run.
const ALL: usize = 100_000;

/// First-person forms that make the SPEAKER a subject of the turn.
///
/// Deliberately narrow. «**mi** ricordi che macchina ha X?» uses `mi` as the
/// *addressee*, not as a subject — the answer is about X alone — so the
/// unstressed clitics are excluded and only the forms that put the speaker IN
/// the question are listed. Getting this wrong is exactly how the asker's
/// unrelated facts would be invited into a question about somebody else.
const FIRST_PERSON: &[&str] = &[
    // Italian: subject pronoun, tonic object, possessives.
    "io", "me", "mio", "mia", "miei", "mie", // English.
    "i", "my", "mine", "myself",
];

/// One case to measure.
struct Turn {
    sender: String,
    text: String,
    expect: Option<String>,
}

fn load_turns(path: &Path) -> anyhow::Result<Vec<Turn>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--turns {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut f = line.split('\t');
        let (Some(sender), Some(text)) = (f.next(), f.next()) else {
            anyhow::bail!(
                "--turns {}:{}: expected `sender<TAB>turn[<TAB>fact_id]`",
                path.display(),
                n + 1
            );
        };
        let (sender, text) = (sender.trim(), text.trim());
        anyhow::ensure!(
            !sender.is_empty() && !text.is_empty(),
            "--turns {}:{}: empty sender or turn",
            path.display(),
            n + 1
        );
        out.push(Turn {
            sender: sender.to_owned(),
            text: text.to_owned(),
            expect: f
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        });
    }
    anyhow::ensure!(!out.is_empty(), "--turns {}: empty", path.display());
    Ok(out)
}

/// Lowercased word tokens — the unit both the subject match and the
/// first-person test work on, so neither ever fires on a substring.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Every name this person answers to, lowercased.
fn names_of(user: &enrollment::EnrolledUserLite) -> Vec<String> {
    let mut v = vec![user.user_id.to_lowercase()];
    v.extend(user.aliases.iter().map(|a| a.to_lowercase()));
    v
}

/// The people this turn is about: the speaker when the first person puts them
/// in the question, plus every enrolled person the turn names.
///
/// A match against the roster, not a judgement — the enrolled identities are a
/// short known list, so this costs a set lookup and no model call.
fn subjects_of(turn: &Turn, roster: &[enrollment::EnrolledUserLite]) -> BTreeSet<String> {
    let w: BTreeSet<String> = words(&turn.text).into_iter().collect();
    let mut subjects = BTreeSet::new();
    if FIRST_PERSON.iter().any(|p| w.contains(*p)) {
        subjects.insert(turn.sender.to_lowercase());
    }
    for u in roster {
        if names_of(u).iter().any(|n| w.contains(n)) {
            subjects.insert(u.user_id.to_lowercase());
        }
    }
    subjects
}

/// The people a FACT is about — governance and content together, because
/// neither alone is aboutness: `owner`/`allow` say who may READ it, and the
/// text and topics say who it NAMES. The measured example needs both at once —
/// the answering fact is owned by one person and names the other only in its
/// topics and its prose.
fn mentions_of(row: &FactIndexRow, roster: &[enrollment::EnrolledUserLite]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut push = |p: &Principal| {
        if let Principal::User(u) = p {
            out.insert(u.to_lowercase());
        }
    };
    push(&row.owner_id);
    for a in &row.allow_ids {
        push(a);
    }
    if let Some(s) = row.sender_id.as_ref() {
        push(s);
    }
    let mut hay: BTreeSet<String> = words(&row.text).into_iter().collect();
    for t in &row.topics {
        hay.extend(words(t));
    }
    for u in roster {
        if names_of(u).iter().any(|n| hay.contains(n)) {
            out.insert(u.user_id.to_lowercase());
        }
    }
    out
}

/// Substitute the first person with the speaker's own id, so the string being
/// matched carries what the engine already knows for certain.
fn resolve_first_person(text: &str, sender: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            if FIRST_PERSON.contains(&word.to_lowercase().as_str()) {
                out.push_str(sender);
            } else {
                out.push_str(word);
            }
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// How many of the top-k are about the ASKER and about nobody else the turn
/// asked about — the crowding number a variant must not inflate.
fn asker_only(
    hits: &[(f32, &RecallHit)],
    top_k: usize,
    sender: &str,
    subjects: &BTreeSet<String>,
    mentions: &HashMap<String, BTreeSet<String>>,
) -> usize {
    hits.iter()
        .take(top_k)
        .filter(|(_, h)| {
            mentions.get(&h.fact_id.to_string()).is_some_and(|m| {
                m.contains(&sender.to_lowercase()) && m.intersection(subjects).count() <= 1
            })
        })
        .count()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::from("./work");
    let mut turns_path: Option<PathBuf> = None;
    let mut top_k = 5usize;
    let mut weights: Vec<f32> = vec![0.02, 0.05, 0.10];
    let mut mults: Vec<f32> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--turns" => turns_path = args.next().map(PathBuf::from),
            "--top-k" => top_k = args.next().unwrap_or_default().parse().unwrap_or(5),
            "--weights" => {
                weights = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|w| w.trim().parse::<f32>().ok())
                    .collect();
            },
            "--mults" => {
                mults = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|w| w.trim().parse::<f32>().ok())
                    .collect();
            },
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    let turns_path = turns_path.ok_or_else(|| anyhow::anyhow!("--turns <file> is required"))?;
    let turns = load_turns(&turns_path)?;
    anyhow::ensure!(!weights.is_empty(), "--weights: at least one");

    let pool = db::open_or_init(&workdir).await?;
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &mwe_core::local_embedder::default_cache_dir("bge-m3"),
        candle_core::Device::Cpu,
        "bge-m3",
    )?);
    let roster: Vec<enrollment::EnrolledUserLite> = enrollment::list_users(&pool)
        .await?
        .into_iter()
        .filter(|u| !u.is_agent)
        .collect();
    println!(
        "roster : {:?}",
        roster.iter().map(|u| &u.user_id).collect::<Vec<_>>()
    );

    // One read of the corpus, only to learn who each fact is about — the
    // ranking itself always comes back through the real search.
    let all = fact_index::find_by_filters(&pool, &fact_index::FactFilters::default()).await?;
    let mentions: HashMap<String, BTreeSet<String>> = all
        .iter()
        .map(|r| (r.fact_id.to_string(), mentions_of(r, &roster)))
        .collect();
    println!("corpus : {} active facts\n", all.len());

    let mut labels: Vec<String> = vec!["base".to_owned(), "rewrite".to_owned()];
    labels.extend(weights.iter().map(|w| format!("cover:{w:.2}")));
    labels.extend(mults.iter().map(|m| format!("mult:{m:.2}")));
    let mut ranks: HashMap<String, Vec<Option<usize>>> = HashMap::new();
    let mut crowding: HashMap<String, usize> = HashMap::new();
    // How many of the base top-k each variant displaced, summed — the
    // disturbance a variant costs on turns it was never meant to help.
    let mut churn: HashMap<String, usize> = HashMap::new();
    let mut slots = 0usize;

    for turn in &turns {
        let sender = SenderContext {
            sender_id: turn.sender.clone(),
            sender_groups: enrollment::groups_for(&pool, &turn.sender).await?,
        };
        let subjects = subjects_of(turn, &roster);
        println!("── «{}»", turn.text);
        println!(
            "   sender={} subjects={:?}",
            turn.sender,
            subjects.iter().collect::<Vec<_>>()
        );

        let base = wiki_search_unrecorded(
            &pool,
            Arc::clone(&embedder),
            &turn.text,
            ALL,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await?;
        let rewritten = resolve_first_person(&turn.text, &turn.sender);
        let rewrite = if rewritten == turn.text {
            base.clone()
        } else {
            println!("   rewrite → «{rewritten}»");
            wiki_search_unrecorded(
                &pool,
                Arc::clone(&embedder),
                &rewritten,
                ALL,
                fact_index::FactFilters::default(),
                &sender,
            )
            .await?
        };

        let base_top: BTreeSet<String> = base
            .iter()
            .take(top_k)
            .map(|h| h.fact_id.to_string())
            .collect();
        for label in &labels {
            let source = if label == "rewrite" { &rewrite } else { &base };
            let mut scored: Vec<(f32, &RecallHit)> = source
                .iter()
                .map(|h| {
                    // The engine already applied its own additive bonus, so
                    // peel it off first: every variant must start from the
                    // same raw cosine or the comparison is against a moving
                    // baseline.
                    let extra = mentions
                        .get(&h.fact_id.to_string())
                        .map_or(0, |m| m.intersection(&subjects).count())
                        .saturating_sub(1) as f32;
                    let raw = h.score / SUBJECT_COVERAGE_UPLIFT.mul_add(extra, 1.0);
                    let s = if let Some(w) = label.strip_prefix("cover:") {
                        raw + w.parse::<f32>().unwrap_or(0.0) * extra
                    } else if let Some(m) = label.strip_prefix("mult:") {
                        raw * m.parse::<f32>().unwrap_or(0.0).mul_add(extra, 1.0)
                    } else {
                        raw
                    };
                    (s, h)
                })
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Less));

            let rank = turn.expect.as_ref().and_then(|want| {
                scored
                    .iter()
                    .position(|(_, h)| h.fact_id.to_string() == *want)
                    .map(|p| p + 1)
            });
            ranks.entry(label.clone()).or_default().push(rank);
            let only = asker_only(&scored, top_k, &turn.sender, &subjects, &mentions);
            *crowding.entry(label.clone()).or_default() += only;
            let kept = scored
                .iter()
                .take(top_k)
                .filter(|(_, h)| base_top.contains(&h.fact_id.to_string()))
                .count();
            *churn.entry(label.clone()).or_default() += top_k - kept;

            println!(
                "   {label:<11} answer@{:<5} asker-only {only}/{top_k}",
                rank.map_or_else(|| ">all".to_owned(), |r| r.to_string())
            );
            if label == "base" || rank.is_some_and(|r| r <= top_k) {
                for (i, (s, h)) in scored.iter().take(top_k).enumerate() {
                    let mark = if turn.expect.as_ref() == Some(&h.fact_id.to_string()) {
                        "★"
                    } else {
                        " "
                    };
                    println!(
                        "       {mark}{}. {s:.4} {}",
                        i + 1,
                        h.text.chars().take(88).collect::<String>()
                    );
                }
            }
        }
        slots += top_k;
        println!();
    }

    println!("════ SUMMARY over {} turns ════", turns.len());
    println!(
        "{:<12} {:>10} {:>10} {:>17} {:>10}",
        "variant", "hit@k", "mean rank", "asker-only share", "churn"
    );
    for label in &labels {
        let labelled: Vec<usize> = ranks[label].iter().filter_map(|r| *r).collect();
        let hit = labelled.iter().filter(|r| **r <= top_k).count();
        let mean = if labelled.is_empty() {
            f32::NAN
        } else {
            labelled.iter().sum::<usize>() as f32 / labelled.len() as f32
        };
        println!(
            "{label:<12} {:>10} {mean:>10.2} {:>16.1}% {:>9.1}%",
            format!("{hit}/{}", labelled.len()),
            100.0 * crowding[label] as f32 / slots as f32,
            100.0 * churn[label] as f32 / slots as f32
        );
    }
    Ok(())
}
