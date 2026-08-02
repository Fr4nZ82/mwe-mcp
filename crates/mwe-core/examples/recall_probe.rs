// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-side diagnostic for planning card 61 — where the two corpora sit
//! relative to each other, and what the merged view actually returns.
//!
//! Throwaway measurement harness. Point it at a **copy** of a workdir (it
//! bumps recall counters through the real orchestrators) and it reports,
//! per probe query:
//!
//! - the full-corpus cosine distribution of facts vs sections, as the
//!   sender actually sees them (both pulled through the real ACL-filtering
//!   orchestrators at an unbounded `top_k`);
//! - how many sections outscore the sender's *best* fact — the structural
//!   crowd-out number;
//! - `search_all` (the merged view `wiki_search`/MCP and `wiki_navigate`
//!   serve) with per-hit corpus labels;
//! - where a known-good fact ("the needle") ranks in the facts-only list;
//! - whether the lexical pass fired, and on what.
//!
//! ```text
//! cargo run -p mwe-core --example recall_probe --features local-embedder --release -- \
//!     --workdir <copy> --sender <user-id> --probes <file> --needle "<a phrase>"
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mwe_core::embedder::Embedder;
use mwe_core::recall::{
    SearchHit, SenderContext, cosine_similarity, search_all, search_sections, wiki_search,
};
use mwe_core::{db, enrollment, fact_index, sections};

const ALL: usize = 100_000;

/// The probe queries, read from a file — one turn per line, `#` comments and
/// blank lines ignored. They stay OUT of this repo on purpose: a useful probe
/// set is verbatim traffic from the corpus under measurement, which is the
/// operator's data and nobody else's.
///
/// A serviceable set mixes four kinds, and the report is only readable if all
/// four are present: the failing turn verbatim · the searches the consumer
/// itself made on that turn · controls that must NOT reach a project ·
/// turns that must.
fn load_probes(path: &Path) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--probes {}: {e}", path.display()))?;
    let probes: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    anyhow::ensure!(!probes.is_empty(), "--probes {}: empty", path.display());
    Ok(probes)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::from("./work");
    let mut sender_id = String::new();
    let mut needle: Option<String> = None;
    let mut top_k = 10usize;
    let mut probes_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--sender" => sender_id = args.next().unwrap_or_default(),
            "--needle" => needle = args.next(),
            "--probes" => probes_path = args.next().map(PathBuf::from),
            "--top-k" => top_k = args.next().unwrap_or_default().parse().unwrap_or(10),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(!sender_id.is_empty(), "--sender <user-id> is required");
    let probes_path = probes_path.ok_or_else(|| anyhow::anyhow!("--probes <file> is required"))?;
    let probes = load_probes(&probes_path)?;

    let pool = db::open_or_init(&workdir).await?;
    let cache = mwe_core::local_embedder::default_cache_dir("bge-m3");
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &cache,
        candle_core::Device::Cpu,
        "bge-m3",
    )?);

    let sender_groups = enrollment::groups_for(&pool, &sender_id).await?;
    println!("sender  : {sender_id}  groups={sender_groups:?}");
    let sender = SenderContext {
        sender_id: sender_id.clone(),
        sender_groups,
    };
    let registry = sections::list_smart_wikis(&pool).await?;
    let all_wikis: Vec<String> = registry.iter().map(|w| w.wiki_id.clone()).collect();

    let pct = |v: &[f32], p: f32| -> f32 {
        if v.is_empty() {
            return f32::NAN;
        }
        v[((v.len() as f32 - 1.0) * p) as usize]
    };

    // ---- the wiki-level gate: would a per-wiki card decide before the scan? --
    // Two candidate signals per readable smart wiki:
    //   card     — what the wiki says it is (`_meta.md` title + `index.md` head)
    //   centroid — the mean of its own section embeddings, needing no authoring
    let mut gate: Vec<(String, Vec<f32>, Vec<f32>, Vec<f32>)> = Vec::new();
    for w in &all_wikis {
        let secs = sections::find_candidates_in_wikis(&pool, std::slice::from_ref(w)).await?;
        if secs.is_empty() {
            continue;
        }
        let dim = secs[0].embedding.len();
        let mut centroid = vec![0.0f32; dim];
        for s in &secs {
            for (c, e) in centroid.iter_mut().zip(&s.embedding) {
                *c += *e;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        for c in &mut centroid {
            *c /= secs.len() as f32;
        }
        // The card: title from `_meta.md`, plus the opening of `index.md`.
        let dir = secs[0]
            .source_path
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_owned());
        let root = workdir.join(&dir);
        let mut card = String::new();
        for up in [
            root.clone(),
            root.parent().map(Path::to_path_buf).unwrap_or(root),
        ] {
            if let Ok(meta) = std::fs::read_to_string(up.join("_meta.md")) {
                if meta.contains(w.as_str()) {
                    for l in meta.lines().filter(|l| l.starts_with("title:")) {
                        card.push_str(l.trim_start_matches("title:").trim());
                    }
                    if let Ok(idx) = std::fs::read_to_string(up.join("index.md")) {
                        card.push_str(". ");
                        card.push_str(&idx.chars().take(600).collect::<String>());
                    }
                    break;
                }
            }
        }
        if card.is_empty() {
            card = w.clone();
        }
        // The authored one-line description, as the wiki itself declares it
        // (`_meta.scope`, mirrored into the registry by the push). A wiki that
        // has never declared one falls back to its bare id, which is what the
        // funnel would actually have to work with.
        let written = registry
            .iter()
            .find(|row| &row.wiki_id == w)
            .and_then(|row| row.description.as_deref())
            .unwrap_or(w.as_str());
        gate.push((
            w.clone(),
            embedder.embed(&card).await?,
            centroid,
            embedder.embed(written).await?,
        ));
    }
    println!("\n════ WIKI-LEVEL GATE — one score per project wiki, before any scan ════");
    print!("{:<62}", "query");
    for (w, ..) in &gate {
        print!("{:>13}", w.rsplit('-').next().unwrap_or(w));
    }
    println!();
    for probe in &probes {
        let q = embedder.embed(probe).await?;
        for (label, pick) in [("card", 1usize), ("cent", 2usize), ("SUMM", 3usize)] {
            print!(
                "{:<56}{label:>6}",
                format!("«{probe}»").chars().take(54).collect::<String>()
            );
            let mut best = f32::MIN;
            for (_, c, ce, sm) in &gate {
                let v = match pick {
                    1 => c,
                    2 => ce,
                    _ => sm,
                };
                let s = cosine_similarity(&q, v);
                best = best.max(s);
                print!("{s:>13.3}");
            }
            println!("   max={best:.3}");
        }
    }
    println!();

    for probe in &probes {
        println!("════════════════════════════════════════════════════════════════");
        println!("QUERY  «{probe}»");

        // Everything the sender can see, scored, through the real orchestrators.
        let facts = wiki_search(
            &pool,
            Arc::clone(&embedder),
            probe,
            ALL,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await?;
        let secs = search_sections(&pool, Arc::clone(&embedder), probe, ALL, &sender).await?;

        let mut fs: Vec<f32> = facts.iter().map(|h| h.score).collect();
        let mut ss: Vec<f32> = secs.iter().map(|h| h.score).collect();
        fs.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        ss.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

        println!(
            "  facts    n={:<5} max={:.4} p90={:.4} median={:.4}",
            fs.len(),
            fs.first().copied().unwrap_or(f32::NAN),
            pct(&fs, 0.10),
            pct(&fs, 0.50)
        );
        println!(
            "  sections n={:<5} max={:.4} p90={:.4} median={:.4}",
            ss.len(),
            ss.first().copied().unwrap_or(f32::NAN),
            pct(&ss, 0.10),
            pct(&ss, 0.50)
        );
        let best_fact = fs.first().copied().unwrap_or(f32::MIN);
        let above = ss.iter().filter(|s| **s > best_fact).count();
        println!("  → {above} section(s) outscore the sender's BEST fact ({best_fact:.4})");

        // The merged view a consumer actually gets.
        let merged = search_all(
            &pool,
            Arc::clone(&embedder),
            probe,
            top_k,
            fact_index::FactFilters::default(),
            mwe_core::recall::DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await?;
        let n_sec = merged
            .iter()
            .filter(|h| matches!(h, SearchHit::Section(_)))
            .count();
        println!(
            "  search_all top-{top_k}: {n_sec} sections / {} facts",
            merged.len() - n_sec
        );
        for (i, h) in merged.iter().enumerate() {
            let (kind, wiki, txt, sc) = match h {
                SearchHit::Fact(f) => ("fact", f.wiki_id.as_str(), f.text.as_str(), f.score),
                SearchHit::Section(s) => ("SECT", s.wiki_id.as_str(), s.text.as_str(), s.score),
            };
            println!(
                "    {:2}. {sc:.4} [{kind}] {wiki:<22} {}",
                i + 1,
                txt.replace('\n', " ").chars().take(88).collect::<String>()
            );
        }

        // COUNTERFACTUAL A — the same merge WITHOUT the cross-corpus lexical
        // fusion: pure score order, which is what `search_all` documents.
        {
            let mut pure: Vec<(f32, &str, &str, &str)> = facts
                .iter()
                .map(|h| (h.score, "fact", h.wiki_id.as_str(), h.text.as_str()))
                .chain(
                    secs.iter()
                        .map(|h| (h.score, "SECT", h.wiki_id.as_str(), h.text.as_str())),
                )
                .collect();
            pure.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            pure.truncate(top_k);
            let ns = pure.iter().filter(|h| h.1 == "SECT").count();
            println!(
                "  [counterfactual: score-only merge] {ns} sections / {} facts",
                pure.len() - ns
            );
            for (i, (sc, kind, wiki, txt)) in pure.iter().enumerate() {
                println!(
                    "    {:2}. {sc:.4} [{kind}] {wiki:<22} {}",
                    i + 1,
                    txt.replace('\n', " ").chars().take(88).collect::<String>()
                );
            }
        }

        // COUNTERFACTUAL B — would a lexical pass over the FACT corpus have
        // found the needle? Cheap proxy: do the query's terms occur in it?
        if let Some(n) = needle.as_deref() {
            let low = n.to_lowercase();
            if let Some(hit) = facts.iter().find(|h| h.text.to_lowercase().contains(&low)) {
                let body = hit.text.to_lowercase();
                let terms: Vec<&str> = probe.split_whitespace().collect();
                let matched: Vec<&str> = terms
                    .iter()
                    .filter(|t| body.contains(&t.to_lowercase()))
                    .copied()
                    .collect();
                println!(
                    "  [counterfactual: FTS over facts] query terms present in the needle fact: {matched:?} of {} → {}",
                    terms.len(),
                    if matched.is_empty() {
                        "a lexical pass would NOT have found it"
                    } else {
                        "a lexical pass could have found it"
                    }
                );
            }
        }

        // THE PRODUCTION GATE, as it actually works: a signpost is a FACT on the
        // owner's reserved `projects.md`. The project corpus opens only when one
        // SURFACES in this turn's own fact recall — then the classifier judges.
        {
            let pos = facts
                .iter()
                .position(|h| h.source_path.ends_with("projects.md"));
            match pos {
                Some(p) if p < 5 => println!(
                    "  [gate signpost] un signpost esce al posto {} del richiamo fatti (top_k ingest = 5) → LE WIKI DI PROGETTO SI APRONO (score {:.4})",
                    p + 1,
                    facts[p].score
                ),
                Some(p) => println!(
                    "  [gate signpost] miglior signpost al posto {} — fuori dai 5 dell'ingest → progetti CHIUSI (score {:.4})",
                    p + 1,
                    facts[p].score
                ),
                None => println!("  [gate signpost] nessun signpost nel corpus → progetti CHIUSI"),
            }
        }

        // Where the needle sits in the facts-only ranking.
        if let Some(n) = needle.as_deref() {
            let low = n.to_lowercase();
            match facts
                .iter()
                .position(|h| h.text.to_lowercase().contains(&low))
            {
                Some(p) => println!(
                    "  needle «{n}» → facts-only rank {}/{} (score {:.4})",
                    p + 1,
                    facts.len(),
                    facts[p].score
                ),
                None => println!("  needle «{n}» → absent from the fact corpus"),
            }
            if let Some(p) = secs
                .iter()
                .position(|h| h.text.to_lowercase().contains(&low))
            {
                println!("  needle «{n}» → sections rank {}", p + 1);
            }
        }

        // Did the lexical pass fire, and on which corpus? Two signals:
        // `search_lexical` is OR over every term (noisy); `search_lexical_headings`
        // is AND over the heading chain (the "the query NAMES this section" signal).
        let lex = sections::search_lexical(&pool, &all_wikis, probe, 50).await?;
        let def = sections::search_lexical_headings(&pool, &all_wikis, probe, 50).await?;
        println!(
            "  lexical OR-list: {:2} hit(s){}   |   definition (heading AND): {} hit(s)",
            lex.len(),
            if lex.is_empty() {
                "                                          ".to_owned()
            } else {
                format!(" — top: {:<38}", lex[0].0.rsplit('/').next().unwrap_or(""))
            },
            def.len()
        );
        println!();
    }
    Ok(())
}
