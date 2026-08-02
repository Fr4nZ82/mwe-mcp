// SPDX-License-Identifier: AGPL-3.0-or-later
//! Throwaway: narrate one turn's deterministic recall over a workdir COPY.
//!
//! Everything here runs without an LLM — the flat fact recall, the entry fan,
//! and the candidate cards exactly as the navigator would be handed them. The
//! model's per-hop *choice* is the only part not reproduced, and it is stated
//! as such rather than guessed.
//!
//! ```text
//! cargo run -p mwe-core --example room_walk --features local-embedder --release -- \
//!     --workdir <copy> --sender <user-id> --query "<the turn, verbatim>" --top-k 5
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
use mwe_core::recall::{SenderContext, wiki_recall};
use mwe_core::{db, enrollment, fact_index, recall_nav, wiki};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut workdir = PathBuf::new();
    let mut sender = "franz".to_owned();
    let mut query = String::new();
    let mut top_k = 5usize;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut v = || it.next().expect("flag needs a value");
        match flag.as_str() {
            "--workdir" => workdir = PathBuf::from(v()),
            "--sender" => sender = v(),
            "--query" => query = v(),
            "--top-k" => top_k = v().parse().expect("number"),
            other => panic!("unknown flag {other}"),
        }
    }

    let pool = db::open_or_init(&workdir).await?;
    let tree = wiki::WikiTree::open(&workdir)?;
    let cache = mwe_core::local_embedder::default_cache_dir("bge-m3");
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &cache,
        candle_core::Device::Cpu,
        "bge-m3",
    )?);
    let groups = enrollment::groups_for(&pool, &sender).await?;
    let ctx = SenderContext {
        sender_id: sender.clone(),
        sender_groups: groups.clone(),
    };

    println!("== QUERY: {query:?}   sender={sender} groups={groups:?}\n");

    // 1. The atrium: flat RAG over the whole readable fact corpus.
    let hits = wiki_recall(
        &pool,
        Arc::clone(&embedder),
        &query,
        &[],
        top_k,
        fact_index::FactFilters::default(),
        &ctx,
    )
    .await?;
    println!("-- 1. FLAT RECALL (top {top_k}) — what lands in the block as-is");
    for (i, h) in hits.iter().enumerate() {
        println!(
            "  {}. {:.3}  [{}] {}\n       page: {}",
            i + 1,
            h.score,
            h.wiki_id,
            h.text.chars().take(110).collect::<String>(),
            h.source_path
        );
    }

    // 2. The entry fan: which pages those hits (plus identity) make reachable.
    let owners = vec![];
    let fan = recall_nav::gather_entry_points(&pool, &tree, &ctx, &[], &owners, &hits, &[]).await?;
    println!(
        "\n-- 2. ENTRY FAN — the addresses the walk starts from ({} )",
        fan.len()
    );
    for (i, e) in fan.iter().enumerate() {
        println!(
            "  {}. {:.3} origin={:<12} {} / {}",
            i + 1,
            e.weight,
            format!("{:?}", e.origin),
            e.wiki_id,
            e.page
                .as_ref()
                .map_or_else(|| "(overview)".to_owned(), |p| p.display().to_string())
        );
    }
    Ok(())
}
