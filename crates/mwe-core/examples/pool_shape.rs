// SPDX-License-Identifier: AGPL-3.0-or-later
//! What shape is the candidate pool the navigator is actually offered, and
//! where does `max_candidates` cut it?
//!
//! One traced turn and two counts off the filesystem (41 pages with no links,
//! one page emitting 22) describe the *corpus*; they do not say how often a
//! real turn meets either end of it. This measures the pool itself, over real
//! turns, and answers three questions:
//!
//! - **Does the cap bite, and on what?** Every hop is run with the pool
//!   left uncapped, then the real cap is applied in the report, so what the
//!   truncation *would* have dropped is visible by origin (`link` rail /
//!   entry-point fan / card rail) instead of being invisible by
//!   construction.
//! - **How often is an opened page a dead end?** The funnel already
//!   journals how many candidates each opened page exposed
//!   ([`OpenedPage::discovered`]); a zero is a room with no exits.
//! - **What pool size is the right one?** The uncapped distribution is
//!   the input that question needs, and it cannot be read off a capped run.
//!
//! **The navigator's own choice is not reproduced** — it is the one part of the
//! funnel that needs a model. In its place every hop opens the first
//! `pages_per_hop` candidates *as offered*, which is what the pool ordering is
//! for and what position bias makes the likeliest pick anyway. Everything else
//! — the fan, the link extraction, the ACL projection, the
//! dedup — is the production code path, called, not re-implemented.
//!
//! One thing the harness asserts rather than derives: the ingest turn serves
//! the sender's `cucina.md` in `WHO IS SPEAKING` and hands it to the funnel as
//! already delivered, so the walk never opens it. This runs with that
//! exclusion in place, because the ingest funnel is what is being measured.
//! The engine additionally checks that the page carries a readable
//! fact before serving it; here it is assumed to, which holds for every
//! enrolled person in the live corpus.
//!
//! Read-only. Local embedder, **no API spend**.
//!
//! ```text
//! cargo run -p mwe-core --example pool_shape --features local-embedder --release -- \
//!     --workdir <copy> --turns <file> --top-k 5 --cap 16
//! ```
//!
//! `--turns` is TSV, `#` comments and blank lines ignored:
//!
//! ```text
//! <sender-id> <TAB> the turn, verbatim [ <TAB> the fact id that answers it ]
//! ```
//!
//! The third column is optional: with it, the report also says whether the page
//! holding that fact ever entered the pool, at what position, and whether the
//! cap cut it — the sharpest form of the question 66a asks.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::single_match_else,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    reason = "measurement harness: the report code is written for a human \
              reading numbers once, not for reuse"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use mwe_core::embedder::Embedder;
use mwe_core::llm::{
    CompletionRequest, CompletionResponse, CompletionUsage, FinishReason, LlmBackend,
    Result as LlmResult,
};
use mwe_core::recall::{SenderContext, wiki_search_unrecorded};
use mwe_core::recall_nav::{HopTrace, NavigatorPolicy};
use mwe_core::types::FactId;
use mwe_core::{db, enrollment, fact_index, recall_nav, wiki};

/// Leave the funnel's own truncation off, so the report can apply the real
/// cap itself and see what it would have dropped.
const UNCAPPED: usize = 1_000_000;

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

/// Stand-in for the navigator: open the first `n` candidates exactly as the
/// pool offered them.
///
/// It reads the candidate list back out of the user prompt the funnel just
/// built, so it can only ever pick something the pool actually contained — a
/// hallucinated target would be vetted away by `open_target` here as it is in
/// production, and the walk would silently shrink.
struct OpenTheFirst {
    n: usize,
}

/// The `wiki_id=… page=…` of one candidate line of the funnel's user prompt.
fn parse_candidate_line(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.split_once("wiki_id=")?.1;
    let (wiki_id, rest) = rest.split_once(" page=")?;
    let page = rest.split(" |").next()?.trim();
    Some((
        wiki_id.trim().to_owned(),
        (page != "(overview)").then(|| page.to_owned()),
    ))
}

#[async_trait]
impl LlmBackend for OpenTheFirst {
    fn model_id(&self) -> &'static str {
        "open-the-first"
    }

    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        let picks: Vec<String> = request
            .prompt
            .split("\nCANDIDATES:\n")
            .nth(1)
            .unwrap_or_default()
            .lines()
            .filter_map(parse_candidate_line)
            .take(self.n)
            .map(|(wiki_id, page)| match page {
                Some(p) => format!(r#"{{"wiki_id":"{wiki_id}","page":"{p}"}}"#),
                None => format!(r#"{{"wiki_id":"{wiki_id}"}}"#),
            })
            .collect();
        Ok(CompletionResponse {
            text: format!(
                r#"{{"open":[{}],"done":false,"note":"first {} as offered"}}"#,
                picks.join(","),
                self.n
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        })
    }

    async fn health_check(&self, _probe: &CompletionRequest) -> LlmResult<()> {
        Ok(())
    }
}

/// Origin buckets, in the order [`recall_nav`] ranks them.
fn bucket(origin: &str) -> usize {
    match origin {
        "link" => 0,
        "rag" | "description" | "topic" | "situational" => 1,
        _ => 2,
    }
}

const BUCKET_NAMES: [&str; 3] = ["link", "fan", "card"];

/// `link N | fan N | page N`, zeros included so the columns line up.
fn by_origin(cards: impl Iterator<Item = String>) -> String {
    let mut counts = [0usize; 3];
    for origin in cards {
        counts[bucket(&origin)] += 1;
    }
    BUCKET_NAMES
        .iter()
        .zip(counts)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Running totals across every turn.
#[derive(Default)]
struct Totals {
    turns: usize,
    hops: usize,
    hops_over_cap: usize,
    cut: [usize; 3],
    offered: [usize; 3],
    opens: usize,
    dead_ends: usize,
    discovered: Vec<usize>,
    /// Labelled turns whose answer page entered the pool at all.
    answer_in_pool: usize,
    /// …of those, the ones the cap would have cut.
    answer_cut: usize,
    labelled: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::new();
    let mut turns_path = PathBuf::new();
    let mut top_k = 5usize;
    let mut cap = NavigatorPolicy::default().max_candidates;
    let mut pages_per_hop = NavigatorPolicy::default().pages_per_hop;
    let mut max_hops = NavigatorPolicy::default().max_hops;
    let mut verbose = false;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut v = || it.next().expect("flag needs a value");
        match flag.as_str() {
            "--workdir" => workdir = PathBuf::from(v()),
            "--turns" => turns_path = PathBuf::from(v()),
            "--top-k" => top_k = v().parse()?,
            "--cap" => cap = v().parse()?,
            "--pages-per-hop" => pages_per_hop = v().parse()?,
            "--max-hops" => max_hops = v().parse()?,
            "--verbose" => verbose = true,
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(
        workdir.as_os_str().len() + turns_path.as_os_str().len() > 0,
        "need --workdir and --turns"
    );

    let turns = load_turns(&turns_path)?;
    let pool = db::open_or_init(&workdir).await?;
    let tree = wiki::WikiTree::open(&workdir)?;
    let cache = mwe_core::embedder::default_cache_dir("bge-m3");
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &cache,
        candle_core::Device::Cpu,
        "bge-m3",
    )?);
    let llm = OpenTheFirst { n: pages_per_hop };
    let policy = NavigatorPolicy {
        max_hops,
        pages_per_hop,
        max_candidates: UNCAPPED,
        ..NavigatorPolicy::default()
    };

    println!(
        "== pool shape over {} turns — cap applied in the report, not in the funnel (cap={cap}, \
         pages_per_hop={pages_per_hop}, max_hops={max_hops}, top_k={top_k})\n",
        turns.len()
    );

    let mut groups_cache: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut t = Totals::default();

    for turn in &turns {
        let groups = match groups_cache.get(&turn.sender) {
            Some(g) => g.clone(),
            None => {
                let g = enrollment::groups_for(&pool, &turn.sender).await?;
                groups_cache.insert(turn.sender.clone(), g.clone());
                g
            },
        };
        let ctx = SenderContext {
            sender_id: turn.sender.clone(),
            sender_groups: groups,
        };

        // The page holding the labelled answer, if this turn carries one.
        let answer_page = match &turn.expect {
            Some(fact_id) => fact_index::find_by_id(&pool, &FactId::parse(fact_id)?)
                .await?
                .map(|row| (row.wiki_id, row.source_path)),
            None => None,
        };

        // Unrecorded: the harness must not bump recall counters on the copy,
        // or turn N+1 would be measured against a corpus turn N changed.
        let hits = wiki_search_unrecorded(
            &pool,
            Arc::clone(&embedder),
            &turn.text,
            top_k,
            fact_index::FactFilters::default(),
            &ctx,
        )
        .await?;
        let turn_vector = embedder.embed(&turn.text).await.unwrap_or_default();
        let fan =
            recall_nav::gather_entry_points(&pool, &tree, &ctx, &[], &hits, &[], &turn_vector)
                .await?;
        // What the ingest turn passes: the sender's identity card, already in
        // the block, so the funnel neither offers nor opens it.
        let served = [(turn.sender.clone(), PathBuf::from("cucina.md"))];
        let outcome = recall_nav::navigate(
            &pool,
            &tree,
            &llm,
            &ctx,
            &turn.text,
            &fan,
            &policy,
            // The card's own links are harvested only when a page is opened, and a
            // served page never is — the ingest turn passes them alongside. This
            // harness measures pool shape, not link yield, so it serves none.
            recall_nav::Served {
                pages: &served,
                cards: &[],
            },
        )
        .await?;

        t.turns += 1;
        t.labelled += usize::from(answer_page.is_some());
        let mut answer_seen = false;
        let mut answer_survived = false;

        let head = format!(
            "-- {:?}  sender={}  fan={}  stop={}",
            turn.text.chars().take(72).collect::<String>(),
            turn.sender,
            fan.len(),
            outcome.stop.as_str()
        );
        let mut lines: Vec<String> = Vec::new();

        for (i, hop) in outcome.trace.iter().enumerate() {
            t.hops += 1;
            let n = hop.candidates.len();
            let over = n.saturating_sub(cap);
            if over > 0 {
                t.hops_over_cap += 1;
            }
            for c in &hop.candidates {
                t.offered[bucket(&c.origin)] += 1;
            }
            for c in hop.candidates.iter().skip(cap) {
                t.cut[bucket(&c.origin)] += 1;
            }
            lines.push(format!(
                "   hop {}: pool {n:3} ({})   cut@{cap} {over:3} ({})",
                i + 1,
                by_origin(hop.candidates.iter().map(|c| c.origin.clone())),
                by_origin(hop.candidates.iter().skip(cap).map(|c| c.origin.clone())),
            ));
            if let Some((wiki_id, source_path)) = &answer_page {
                if let Some(pos) = position_of(hop, wiki_id, source_path) {
                    answer_seen = true;
                    let verdict = if pos < cap {
                        answer_survived = true;
                        "survives"
                    } else {
                        "CUT"
                    };
                    lines.push(format!(
                        "        answer page {wiki_id}/{source_path} at #{} — {verdict}",
                        pos + 1
                    ));
                }
            }
            for o in &hop.opened {
                t.opens += 1;
                t.discovered.push(o.discovered);
                if o.discovered == 0 {
                    t.dead_ends += 1;
                }
                lines.push(format!(
                    "        opened {}/{} (+{}{})",
                    o.wiki_id,
                    o.page,
                    o.discovered,
                    if o.discovered == 0 { " DEAD END" } else { "" }
                ));
            }
        }
        if answer_seen {
            t.answer_in_pool += 1;
            if !answer_survived {
                t.answer_cut += 1;
            }
        }
        if verbose
            || lines
                .iter()
                .any(|l| l.contains("CUT") || l.contains("DEAD END"))
        {
            println!("{head}");
            for l in &lines {
                println!("{l}");
            }
            println!();
        }
    }

    t.discovered.sort_unstable();
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            100.0 * n as f32 / d as f32
        }
    };
    let quantile = |q: f32| -> usize {
        if t.discovered.is_empty() {
            0
        } else {
            let idx = ((t.discovered.len() as f32 - 1.0) * q).round() as usize;
            t.discovered[idx]
        }
    };

    println!("== TOTALS");
    println!("turns                       {:5}", t.turns);
    println!("hops observed               {:5}", t.hops);
    println!(
        "hops the cap would cut      {:5}  ({:.1} %)",
        t.hops_over_cap,
        pct(t.hops_over_cap, t.hops)
    );
    let offered: usize = t.offered.iter().sum();
    let cut: usize = t.cut.iter().sum();
    println!(
        "candidates offered          {offered:5}   ({})",
        BUCKET_NAMES
            .iter()
            .zip(t.offered)
            .map(|(n, c)| format!("{n} {c}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "…the cap would cut          {cut:5}  ({:.1} %)   ({})",
        pct(cut, offered),
        BUCKET_NAMES
            .iter()
            .zip(t.cut)
            .map(|(n, c)| format!("{n} {c}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("pages opened                {:5}", t.opens);
    println!(
        "…that exposed nothing       {:5}  ({:.1} %)   ← dead ends",
        t.dead_ends,
        pct(t.dead_ends, t.opens)
    );
    println!(
        "discoveries per open        p50 {}  p90 {}  max {}",
        quantile(0.5),
        quantile(0.9),
        t.discovered.last().copied().unwrap_or(0)
    );
    if t.labelled > 0 {
        println!(
            "labelled turns              {:5}   answer page reached the pool {} — cut by the cap {}",
            t.labelled, t.answer_in_pool, t.answer_cut
        );
    }
    Ok(())
}

/// Position of `wiki_id/source_path` in a hop's pool, if it is there at all.
/// `source_path` is workdir-relative; the pool's page is wiki-relative, so the
/// match is on the tail.
fn position_of(hop: &HopTrace, wiki_id: &str, source_path: &str) -> Option<usize> {
    hop.candidates.iter().position(|c| {
        c.wiki_id == wiki_id
            && c.page
                .as_deref()
                .is_some_and(|p| source_path.ends_with(p) || p.ends_with(source_path))
    })
}
