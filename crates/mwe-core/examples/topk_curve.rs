// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planning card 61 — sizing `recall_top_k` and the missing relevance floor.
//!
//! `recall_top_k` is the one recall knob whose **compute** cost does not move
//! with its value: `wiki_search` scans every candidate vector whatever `k` is,
//! sorts, and truncates at the end. What `k` buys is prompt tokens and, when
//! the turn has nothing to match, injected noise. So the question is not "how
//! many hits can we afford" but "where does the signal actually stop", and
//! that is a property of the score curve on real turns.
//!
//! Reads turns from a file (one per line, `#` comments skipped) and reports,
//! per turn and in aggregate: the score at each rank, the rank-5 cut, and how
//! many hits a given floor would keep.
//!
//! Read-only against a workdir copy. Local embedder, no API spend.
//!
//! ```text
//! cargo run -p mwe-core --example topk_curve --features local-embedder --release -- \
//!     --workdir <copy> --sender franz --turns turns.txt
//! ```

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::collapsible_if,
    clippy::option_if_let_else,
    reason = "throwaway measurement harness: the report code is written for a               human reading numbers once, not for reuse"
)]

use std::path::PathBuf;
use std::sync::Arc;

use mwe_core::embedder::Embedder;
use mwe_core::recall::{SenderContext, wiki_search};
use mwe_core::{db, enrollment, fact_index};

const DEPTH: usize = 25;
const FLOORS: [f32; 5] = [0.40, 0.45, 0.50, 0.55, 0.60];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::from("./work");
    let mut sender_id = "franz".to_string();
    let mut turns_file = PathBuf::from("turns.txt");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--sender" => sender_id = args.next().unwrap_or_default(),
            "--turns" => turns_file = PathBuf::from(args.next().unwrap_or_default()),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    let turns: Vec<String> = std::fs::read_to_string(&turns_file)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect();

    let pool = db::open_or_init(&workdir).await?;
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &mwe_core::local_embedder::default_cache_dir("bge-m3"),
        candle_core::Device::Cpu,
        "bge-m3",
    )?);
    let sender = SenderContext {
        sender_id: sender_id.clone(),
        sender_groups: enrollment::groups_for(&pool, &sender_id).await?,
    };

    let mut curves: Vec<Vec<f32>> = Vec::new();
    let mut kept: Vec<[usize; FLOORS.len()]> = Vec::new();
    let mut chars_1_5 = 0usize;
    let mut chars_6_20 = 0usize;

    for turn in &turns {
        let hits = wiki_search(
            &pool,
            Arc::clone(&embedder),
            turn,
            DEPTH,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await?;
        if hits.is_empty() {
            continue;
        }
        curves.push(hits.iter().map(|h| h.score).collect());
        let mut k = [0usize; FLOORS.len()];
        for (i, f) in FLOORS.iter().enumerate() {
            k[i] = hits.iter().filter(|h| h.score >= *f).count();
        }
        kept.push(k);
        chars_1_5 += hits.iter().take(5).map(|h| h.text.len()).sum::<usize>();
        chars_6_20 += hits
            .iter()
            .skip(5)
            .take(15)
            .map(|h| h.text.len())
            .sum::<usize>();
    }

    let n = curves.len();
    println!("turni misurati: {n}\n");

    // The curve, normalised to rank 1 — where does the signal actually stop?
    println!("── score curve, median over {n} turns ──");
    println!(
        "{:<6}{:>10}{:>14}{:>16}",
        "rango", "punteggio", "% del 1°", "perso vs 1°"
    );
    let med_at = |rank: usize| -> f32 {
        let mut v: Vec<f32> = curves.iter().filter_map(|c| c.get(rank).copied()).collect();
        if v.is_empty() {
            return f32::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let top = med_at(0);
    for r in [0usize, 1, 2, 3, 4, 5, 6, 7, 9, 11, 14, 19, 24] {
        let s = med_at(r);
        if s.is_nan() {
            continue;
        }
        let marker = match r {
            4 => "   ← taglio attuale (recall_top_k = 5)",
            9 => "   ← se fosse 10",
            19 => "   ← se fosse 20",
            _ => "",
        };
        println!(
            "{:<6}{:>10.4}{:>13.0}%{:>15.4}{marker}",
            r + 1,
            s,
            100.0 * s / top,
            top - s
        );
    }

    // The turn-level signal: how strong is the BEST hit? A turn whose top
    // fact is weak has nothing to say, however many rows `top_k` returns.
    let mut tops: Vec<f32> = curves.iter().filter_map(|c| c.first().copied()).collect();
    tops.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    println!("\n── punteggio del PRIMO fatto, distribuzione sui turni reali ──");
    for (label, p) in [
        ("min", 0.0),
        ("5%", 0.05),
        ("10%", 0.10),
        ("25%", 0.25),
        ("median", 0.50),
    ] {
        let i = ((tops.len() - 1) as f32 * p) as usize;
        println!("  {label:<9} {:.4}", tops[i]);
    }
    println!("  (the measured turn whose noise poisoned the answer scored 0.4306)");

    // How many hits survive a floor — i.e. how much of `top_k` is real.
    println!("\n── how many facts would survive a floor ──");
    println!(
        "{:<10}{:>12}{:>16}{:>22}",
        "soglia", "media", "mediana", "turni con 0 fatti"
    );
    for (i, f) in FLOORS.iter().enumerate() {
        let vals: Vec<usize> = kept.iter().map(|k| k[i]).collect();
        let mut sorted = vals.clone();
        sorted.sort_unstable();
        let zeros = vals.iter().filter(|v| **v == 0).count();
        println!(
            "{:<10.2}{:>12.1}{:>16}{:>19} ({:.0}%)",
            f,
            vals.iter().sum::<usize>() as f32 / n as f32,
            sorted[sorted.len() / 2],
            zeros,
            100.0 * zeros as f32 / n as f32
        );
    }

    // What the extra hits cost in prompt.
    println!("\n── costo in prompt dei ranghi 6-20 ──");
    println!(
        "  ranghi 1-5  : {:>7} caratteri  (~{} token)",
        chars_1_5 / n,
        chars_1_5 / n / 4
    );
    println!(
        "  ranghi 6-20 : {:>7} caratteri  (~{} token)  su un prompt di ~28 000 token",
        chars_6_20 / n,
        chars_6_20 / n / 4
    );
    Ok(())
}
