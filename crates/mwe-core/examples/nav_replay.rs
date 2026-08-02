// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planning card 61 — the navigator counterfactual.
//!
//! Replays the funnel on a turn that never got one, against a **copy** of a
//! workdir, and prints the entry fan, every hop's candidate cards, the
//! navigator's picks and note, and the pages it opened. Read-only apart from
//! the section/fact recall-counter bumps the seed search makes on the copy.
//!
//! Costs one navigator completion per hop (`max_hops`, default 2).
//!
//! ```text
//! cargo run -p mwe-core --example nav_replay --features local-embedder --release -- \
//!     --workdir <copy> --sender franz --turn "…"
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use mwe_core::config::{Config, LlmFunction};
use mwe_core::ingest::IngestPolicy;
use mwe_core::recall::SenderContext;
use mwe_core::wiki::WikiTree;
use mwe_core::{db, enrollment, fact_index, oauth, recall, recall_nav};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::from("./work");
    let mut sender_id = "franz".to_string();
    let mut turn = "gandalf metti una playlist che ci piace a me a redacted".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--sender" => sender_id = args.next().unwrap_or_default(),
            "--turn" => turn = args.next().unwrap_or_default(),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }

    let pool = db::open_or_init(&workdir).await?;
    let tree = WikiTree::open(&workdir)?;
    let cfg = Config::load(&workdir)?;

    // The deployment signs in with Claude Code OAuth; install the store so the
    // navigator slot resolves its token. The cached access token is reused
    // whenever it is more than 60 s from expiry — no refresh, no rotation.
    oauth::install_global_store(Arc::new(oauth::OauthStore::new(
        &workdir,
        oauth::OauthClient::new(reqwest::Client::new()),
    )));

    let slot = cfg
        .llm
        .slot(LlmFunction::Navigator)
        .ok_or_else(|| anyhow::anyhow!("no navigator slot in config"))?;
    println!("navigator slot: {} / {}", slot.backend, slot.model);
    let llm = slot.build_backend(LlmFunction::Navigator)?;

    let embedder = cfg.embedding.build_embedder().await?;
    let sender_groups = enrollment::groups_for(&pool, &sender_id).await?;
    let sender = SenderContext {
        sender_id: sender_id.clone(),
        sender_groups: sender_groups.clone(),
    };
    println!("sender: {sender_id} groups={sender_groups:?}");
    println!("turn  : «{turn}»\n");

    // Step 1 — exactly what the live turn computed: flat recall at the policy's
    // own top_k, used as the RAG seeds. The classifier returned no topics and no
    // owners on the real turn, so both stay empty here.
    let policy = IngestPolicy::default();
    let rag = recall::wiki_search(
        &pool,
        Arc::clone(&embedder),
        &turn,
        policy.recall_top_k,
        fact_index::FactFilters::default(),
        &sender,
    )
    .await?;
    println!(
        "── flat recall (the seeds), top_k={} ──",
        policy.recall_top_k
    );
    for (i, h) in rag.iter().enumerate() {
        println!("  {}. {:.4} [{}] {}", i + 1, h.score, h.wiki_id, h.text);
    }

    // Step 2 — the entry fan. Deterministic, no LLM.
    let fan = recall_nav::gather_entry_points(&pool, &tree, &sender, &[], &[], &rag, &[]).await?;
    println!("\n── entry fan ({} seeds) ──", fan.len());
    for e in &fan {
        println!(
            "  {:<28} {:<20} {:?} w={:.3}",
            e.wiki_id,
            e.page.as_deref().map_or_else(
                || "(wiki root)".to_owned(),
                |p| p.to_string_lossy().into_owned()
            ),
            e.origin,
            e.weight
        );
    }

    // Step 3 — the funnel. One navigator completion per hop.
    println!("\n── navigating (max_hops={}) ──", policy.nav.max_hops);
    let out = recall_nav::navigate(
        &pool,
        &tree,
        llm.as_ref(),
        &sender,
        &turn,
        &fan,
        &policy.nav,
    )
    .await?;

    for (i, hop) in out.trace.iter().enumerate() {
        println!("\n═══ HOP {i} ═══");
        println!("  candidates offered ({}):", hop.candidates.len());
        for c in &hop.candidates {
            println!(
                "    - {:<26} {:<34} kw={:?}",
                c.wiki_id,
                c.page.as_deref().unwrap_or("(root)"),
                c.keywords.iter().take(9).collect::<Vec<_>>()
            );
        }
        println!("  navigator note : {}", hop.note.as_deref().unwrap_or("—"));
        println!("  requested      : {:?}", hop.requested);
        println!("  done           : {}", hop.done);
        for o in &hop.opened {
            println!(
                "  OPENED         : {}/{}  ({} chars, {} links found)",
                o.wiki_id, o.page, o.chars, o.discovered
            );
        }
    }

    println!(
        "\n── outcome ── hops={} stop={:?} truncated={} fragments={}",
        out.hops,
        out.stop,
        out.truncated,
        out.fragments.len()
    );
    let mut found = false;
    for f in &out.fragments {
        let hit = f.text.to_lowercase().contains("storie tese");
        if hit {
            found = true;
        }
        println!(
            "\n  ({}/{})  {}{}",
            f.wiki_id,
            f.page.to_string_lossy(),
            if hit {
                "★ CONTIENE «storie tese» ★\n"
            } else {
                ""
            },
            f.text.chars().take(700).collect::<String>()
        );
    }
    println!(
        "\n════ VERDETTO: la risposta {} nel contesto navigato ════",
        if found {
            "È PRESENTE"
        } else {
            "NON è presente"
        }
    );
    Ok(())
}
