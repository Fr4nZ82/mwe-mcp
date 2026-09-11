// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Is a page found by what it IS, and where does the floor go?**
//!
//! The flat search compares a turn to the text of every fact, so a page whose
//! facts are short entries is unreachable through it: a shopping list says
//! «Latte è necessario.» thirteen times and nothing in it resembles *«mi
//! scrivi la lista della spesa?»*. The description family of the entry-point
//! fan (`recall_nav`) compares the turn to the page's **description**
//! instead. This measures whether that works on a real corpus, and it is
//! where `recall_nav::DESCRIPTION_FLOOR` was read off.
//!
//! Per probe phrase, all deterministic and **free** — local embedder,
//! `wiki_search_unrecorded` so no recall counter moves, **no LLM call, no
//! spend**:
//!
//! - the nearest page descriptions with their cosine, **ungated** — the raw
//!   shape of the vector space, so the gap between the description the
//!   question is about and the rest of the memory is a number rather than an
//!   assertion;
//! - whether the flat search reaches the same page on its facts alone — the
//!   before/after of the whole change;
//! - the fan the gatherer actually produces for that sender, so the door is
//!   read where recall reads it and not in a spreadsheet.
//!
//! Running the same probes as two senders is how the ACL gate is read: the
//! ungated ranking is identical for both, and the page leaves the fan for the
//! one whose read-set does not cover the wiki.
//!
//! Point it at a **copy** of a workdir.
//!
//! ```text
//! cargo run --release --features local-embedder --example description_doors -- \
//!     --workdir /path/to/copy --sender alice --probes probes.txt
//! ```
//!
//! Probe file: one phrase per line; blank lines and `#` comments skipped. A
//! line may name the page it should find after a `|`:
//! `mi scrivi la lista della spesa? | famiglia/spesa.md`

use std::path::PathBuf;
use std::sync::Arc;

use mwe_core::embedder::Embedder;
use mwe_core::recall::SenderContext;
use mwe_core::{db, enrollment, fact_index, page_card, recall, recall_nav, wiki};

/// How many nearest descriptions to print per probe.
const SHOW: usize = 8;

struct Args {
    workdir: PathBuf,
    sender: String,
    probes: PathBuf,
    top_k: usize,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut workdir = None;
    let mut sender = None;
    let mut probes = None;
    let mut top_k = 10usize;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--workdir" => workdir = it.next().map(PathBuf::from),
            "--sender" => sender = it.next(),
            "--probes" => probes = it.next().map(PathBuf::from),
            "--top-k" => top_k = it.next().and_then(|v| v.parse().ok()).unwrap_or(top_k),
            other => anyhow::bail!("unknown flag `{other}`"),
        }
    }
    Ok(Args {
        workdir: workdir.ok_or_else(|| anyhow::anyhow!("--workdir is required"))?,
        sender: sender.ok_or_else(|| anyhow::anyhow!("--sender is required"))?,
        probes: probes.ok_or_else(|| anyhow::anyhow!("--probes is required"))?,
        top_k,
    })
}

/// One probe line: the phrase, and the page it is expected to find.
struct Probe {
    phrase: String,
    expect: Option<String>,
}

fn read_probes(path: &PathBuf) -> anyhow::Result<Vec<Probe>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('|') {
            Some((phrase, expect)) => Probe {
                phrase: phrase.trim().to_owned(),
                expect: Some(expect.trim().to_owned()),
            },
            None => Probe {
                phrase: l.to_owned(),
                expect: None,
            },
        })
        .collect())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let probes = read_probes(&args.probes)?;

    let pool = db::open_or_init(&args.workdir).await?;
    let tree = wiki::WikiTree::open(&args.workdir)?;
    let cache = mwe_core::embedder::default_cache_dir("bge-m3");
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &cache,
        candle_core::Device::Cpu,
        "bge-m3",
    )?);

    let sender_groups = enrollment::groups_for(&pool, &args.sender).await?;
    let sender = SenderContext {
        sender_id: args.sender.clone(),
        sender_groups,
    };

    let cards = page_card::all_embedded(&pool).await?;
    println!("== description doors ==");
    println!("workdir : {}", args.workdir.display());
    println!("sender  : {} {:?}", sender.sender_id, sender.sender_groups);
    println!("cards   : {} pages carry a description vector", cards.len());
    println!("floor   : {}\n", recall_nav::DESCRIPTION_FLOOR);

    for probe in &probes {
        println!("── {}", probe.phrase);
        let q = embedder.embed(&probe.phrase).await?;

        let mut scored: Vec<(f32, &str)> = cards
            .iter()
            .map(|c| {
                (
                    recall::cosine_similarity(&q, &c.embedding),
                    c.source_path.as_str(),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!("   nearest descriptions:");
        for (score, path) in scored.iter().take(SHOW) {
            println!("     {score:.4}  {path}");
        }
        if let Some(expect) = &probe.expect {
            match scored.iter().position(|(_, p)| p.ends_with(expect)) {
                Some(rank) => println!(
                    "   expected {expect} ranks #{} at {:.4}",
                    rank + 1,
                    scored[rank].0
                ),
                None => println!("   expected {expect} carries no description vector"),
            }
        }

        // What the flat search reaches on the facts alone — the behaviour the
        // description family exists to fix.
        let flat = recall::wiki_search_unrecorded(
            &pool,
            Arc::clone(&embedder),
            &probe.phrase,
            args.top_k,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await?;
        let flat_pages: Vec<&str> = flat.iter().map(|h| h.source_path.as_str()).collect();
        match probe.expect.as_ref() {
            Some(expect) => println!(
                "   flat top-{}: {} the expected page",
                args.top_k,
                if flat_pages.iter().any(|p| p.ends_with(expect)) {
                    "reaches"
                } else {
                    "never reaches"
                }
            ),
            None => println!("   flat top-{}: {} hits", args.top_k, flat.len()),
        }

        let fan = recall_nav::gather_entry_points_with_descriptions(
            &pool,
            &tree,
            &sender,
            &[],
            &recall_nav::hits_as_doors(&flat, args.top_k),
            &[],
            &q,
        )
        .await?;
        println!("   fan ({} doors):", fan.len());
        for ep in fan.iter().take(SHOW) {
            println!(
                "     {:.4}  {:<12} {}/{}",
                ep.weight,
                format!("{:?}", ep.origin),
                ep.wiki_id,
                ep.page.display()
            );
        }
        println!();
    }
    Ok(())
}
