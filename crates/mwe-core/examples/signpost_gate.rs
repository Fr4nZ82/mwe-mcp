// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planning card 61 — choosing the smart-wiki admission rule on evidence.
//!
//! The founder's contract: *the project corpus stays out of recall unless the
//! turn is explicitly about a project*, and the **signpost description** is the
//! funnel that decides. This harness evaluates the candidate rules against a
//! probe set with both halves — turns that must NOT open a project, and turns
//! that MUST — so the threshold is measured rather than guessed.
//!
//! Read-only. Local embedder, no API spend.
//!
//! ```text
//! cargo run -p mwe-core --example signpost_gate --features local-embedder --release -- \
//!     --workdir <copy> --sender <user-id> --cases <file>
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mwe_core::embedder::Embedder;
use mwe_core::recall::{SenderContext, cosine_similarity, wiki_search};
use mwe_core::{db, enrollment, fact_index, sections, signposts};

/// The labelled probe set, read from a file. It stays OUT of this repo on
/// purpose: the only probe set worth measuring is verbatim traffic from the
/// corpus under test, which is the operator's data and nobody else's.
///
/// One case per line, `#` comments and blank lines ignored:
///
/// ```text
/// personal <TAB> the turn, verbatim          # must leave every project shut
/// project  <TAB> <wiki-slug> <TAB> the turn  # must open exactly that project
/// ```
///
/// **Both halves have to be present.** A set of only-personal turns scores a
/// perfect 100 % for a gate that is welded shut, which is the wrong answer
/// arrived at convincingly.
fn load_cases(path: &Path) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--cases {}: {e}", path.display()))?;
    let mut cases: Vec<(String, Option<String>)> = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split('\t').map(str::trim);
        match (f.next(), f.next(), f.next()) {
            (Some("personal"), Some(turn), None) if !turn.is_empty() => {
                cases.push((turn.to_owned(), None));
            },
            (Some("project"), Some(slug), Some(turn)) if !slug.is_empty() && !turn.is_empty() => {
                cases.push((turn.to_owned(), Some(slug.to_owned())));
            },
            _ => anyhow::bail!(
                "--cases {}:{}: expected `personal<TAB>turn` or `project<TAB>slug<TAB>turn`",
                path.display(),
                n + 1
            ),
        }
    }
    let projects = cases.iter().filter(|(_, w)| w.is_some()).count();
    anyhow::ensure!(
        projects > 0 && projects < cases.len(),
        "--cases {}: needs both halves — {} personal, {projects} project",
        path.display(),
        cases.len() - projects
    );
    Ok(cases)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::from("./work");
    let mut sender_id = String::new();
    let mut cases_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--sender" => sender_id = args.next().unwrap_or_default(),
            "--cases" => cases_path = args.next().map(PathBuf::from),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(!sender_id.is_empty(), "--sender <user-id> is required");
    let cases_path = cases_path.ok_or_else(|| anyhow::anyhow!("--cases <file> is required"))?;
    let cases = load_cases(&cases_path)?;
    let n_project = cases.iter().filter(|(_, w)| w.is_some()).count();
    let n_personal = cases.len() - n_project;

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

    // Every readable smart wiki, and the signpost description it has (if any).
    let registry = sections::list_smart_wikis(&pool).await?;
    let all_facts = fact_index::find_by_filters(&pool, &fact_index::FactFilters::default()).await?;
    let mut desc: BTreeMap<String, (String, Vec<f32>)> = BTreeMap::new();
    for row in &all_facts {
        if row.fact_type.as_deref() != Some(signposts::SIGNPOST_FACT_TYPE) {
            continue;
        }
        let Some(project) = signposts::project_of(row) else {
            continue;
        };
        // Descriptions only: an activity line carries a day topic.
        if !row.topics.iter().any(|t| t == "signpost-description") {
            continue;
        }
        desc.insert(project, (row.text.clone(), row.embedding.clone()));
    }

    println!("smart wikis readable: {}", registry.len());
    for w in &registry {
        println!(
            "  {:<24} riepilogo: {}",
            w.wiki_id,
            desc.get(&w.wiki_id).map_or_else(
                || "— ASSENTE —".to_owned(),
                |(t, _)| format!("{}…", t.chars().take(60).collect::<String>())
            )
        );
    }
    println!();

    let floors = [0.30f32, 0.35, 0.40, 0.45, 0.50];
    let ranks = [5usize, 10, 20];

    // (rule label, personal false-opens, project true-opens, project misses)
    let mut score: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();

    let mut run = |label: String| {
        score.entry(label).or_insert((0, 0, 0));
    };
    for f in floors {
        run(format!("SIGN>={f:.2}"));
    }
    for k in ranks {
        run(format!("RANK<={k}"));
    }
    run("NAME only".to_owned());

    for (probe, want) in &cases {
        let q = embedder.embed(probe).await?;
        // The fact ranking this turn produces anyway — the signpost competes in it.
        let facts = wiki_search(
            &pool,
            Arc::clone(&embedder),
            probe,
            50,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await?;
        let sign_rank: BTreeMap<String, usize> = facts
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                desc.iter()
                    .find(|(_, (t, _))| *t == h.text)
                    .map(|(w, _)| (w.clone(), i + 1))
            })
            .collect();

        let named: Vec<String> = registry
            .iter()
            .filter(|w| {
                let slug = w.slug.to_lowercase();
                !slug.is_empty()
                    && slug.len() >= 4
                    && probe.to_lowercase().contains(&slug.replace('-', " "))
                    || (!slug.is_empty() && slug.len() >= 4 && probe.to_lowercase().contains(&slug))
            })
            .map(|w| w.wiki_id.clone())
            .collect();

        println!(
            "── «{probe}»  (expected: {})",
            want.as_deref().unwrap_or("NONE")
        );
        let mut sims: Vec<(String, f32)> = desc
            .iter()
            .map(|(w, (_, e))| (w.clone(), cosine_similarity(&q, e)))
            .collect();
        sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        print!("   description similarity:");
        for (w, s) in sims.iter().take(3) {
            print!("  {}={s:.3}", w.rsplit('-').next().unwrap_or(w));
        }
        print!("   | rank among facts:");
        if sign_rank.is_empty() {
            print!(" none in the top 50");
        }
        for (w, r) in &sign_rank {
            print!(" {}=#{r}", w.rsplit('-').next().unwrap_or(w));
        }
        println!("   | named: {named:?}");

        let mut tally = |label: String, admitted: Vec<String>| {
            let e = score.get_mut(&label).expect("rule");
            match want.as_deref() {
                None => e.0 += usize::from(!admitted.is_empty()),
                Some(w) => {
                    if admitted.iter().any(|a| a.ends_with(w)) {
                        e.1 += 1;
                    } else {
                        e.2 += 1;
                    }
                },
            }
        };
        for f in floors {
            let adm: Vec<String> = sims
                .iter()
                .filter(|(_, s)| *s >= f)
                .map(|(w, _)| w.clone())
                .chain(named.iter().cloned())
                .collect();
            tally(format!("SIGN>={f:.2}"), adm);
        }
        for k in ranks {
            let adm: Vec<String> = sign_rank
                .iter()
                .filter(|(_, r)| **r <= k)
                .map(|(w, _)| w.clone())
                .chain(named.iter().cloned())
                .collect();
            tally(format!("RANK<={k}"), adm);
        }
        tally("NAME only".to_owned(), named.clone());
    }

    println!("\n════ RULE OUTCOMES ({n_personal} personal / {n_project} project) ════");
    println!(
        "{:<12} {:>26} {:>16} {:>16}",
        "rule", "false opens (on personal)", "projects hit", "projects missed"
    );
    for (label, (fp, tp, fnn)) in &score {
        println!("{label:<12} {fp:>22} {tp:>18} {fnn:>16}");
    }
    Ok(())
}
