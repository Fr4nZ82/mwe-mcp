//! Planning card 69d — **should a third party's identity card be served
//! deterministically, and on what condition?**
//!
//! The founder's question, 2026-08-04: the speaker's card is always in the
//! block, another person's is not — let the funnel find it, or put it there?
//! Serving and navigating are the **same act decided once** (the served page
//! is handed to `navigate` as already-visited, so it can never also be
//! opened), which makes the gate the only lever there is. This measures the
//! gate.
//!
//! Per probe phrase it reports, all deterministic and **free** — local
//! embedder, `wiki_search_unrecorded` so no recall counter moves, **no LLM
//! call, no spend**:
//!
//! - what [`recall::turn_subjects`] resolves (today's gate: a word match over
//!   the enrolled roster, plus the speaker on a first-person pronoun);
//! - the flat recall top-K as that sender actually sees it, ACL included —
//!   i.e. what the turn already gets **without** any card being served;
//! - whether the phrase's `--expect` needles are in there, and at what rank.
//!   This is the founder's own success test: *«"sta sera cucino io, cosa
//!   faccio per carol?" deve venir fuori la celiachia e la gravidanza come
//!   minimo»*;
//! - what serving each resolved subject's card would cost in characters, and
//!   how many of its facts the flat slot already carries — the double-pay the
//!   gate has to avoid.
//!
//! Point it at a **copy** of a workdir.
//!
//! ```text
//! cargo run --release --features local-embedder --example subject_gate -- \
//!     --workdir /path/to/copy --sender franz --probes probes.txt
//! ```
//!
//! Probe file: one phrase per line; blank lines and `#` comments skipped.
//! A line may carry expected needles after a `|`, comma-separated:
//! `sta sera cucino io, cosa faccio per carol? | celiac, gravidanza`

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mwe_core::embedder::Embedder;
use mwe_core::llm::{
    CompletionRequest, CompletionResponse, CompletionUsage, FinishReason, LlmBackend,
    Result as LlmResult,
};
use mwe_core::recall::SenderContext;
use mwe_core::recall_nav::NavigatorPolicy;
use mwe_core::{db, enrollment, fact_index, recall, recall_nav, wiki};

/// Stand-in navigator: opens the first `n` candidates **as offered**, so the
/// walk is driven by the ordering under examination and **no model is called**
/// — the whole harness stays free. Lifted from `pool_shape.rs`, same contract:
/// it measures what the funnel *offers*, never what a model would choose.
struct OpenTheFirst {
    n: usize,
}

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
            .map(|(wiki_id, page)| {
                page.map_or_else(
                    || format!(r#"{{"wiki_id":"{wiki_id}"}}"#),
                    |p| format!(r#"{{"wiki_id":"{wiki_id}","page":"{p}"}}"#),
                )
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
}

struct Probe {
    phrase: String,
    expect: Vec<String>,
}

fn load_probes(path: &PathBuf) -> anyhow::Result<Vec<Probe>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('|') {
            Some((p, e)) => Probe {
                phrase: p.trim().to_owned(),
                expect: e
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect(),
            },
            None => Probe {
                phrase: l.to_owned(),
                expect: Vec::new(),
            },
        })
        .collect())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut workdir = PathBuf::new();
    let mut sender_id = String::new();
    let mut probes_path: Option<PathBuf> = None;
    let mut top_k = 10usize;
    let mut sibling_floor = NavigatorPolicy::default().sibling_floor;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--workdir" => workdir = PathBuf::from(args.next().unwrap_or_default()),
            "--sender" => sender_id = args.next().unwrap_or_default(),
            "--probes" => probes_path = args.next().map(PathBuf::from),
            "--top-k" => top_k = args.next().unwrap_or_default().parse().unwrap_or(10),
            // A/B the 2026-08-04 ruling: restore the directory listing to see
            // what turning it off costs a given phrase.
            "--siblings" => sibling_floor = args.next().unwrap_or_default().parse().unwrap_or(0),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(!sender_id.is_empty(), "--sender <user-id> is required");
    let probes_path = probes_path.ok_or_else(|| anyhow::anyhow!("--probes <file> is required"))?;
    let probes = load_probes(&probes_path)?;

    let pool = db::open_or_init(&workdir).await?;
    let tree = wiki::WikiTree::open(&workdir)?;
    let cache = mwe_core::local_embedder::default_cache_dir("bge-m3");
    let embedder: Arc<dyn Embedder> = Arc::new(mwe_core::local_embedder::LocalEmbedder::load(
        &cache,
        candle_core::Device::Cpu,
        "bge-m3",
    )?);

    let sender_groups = enrollment::groups_for(&pool, &sender_id).await?;
    let sender = SenderContext {
        sender_id: sender_id.clone(),
        sender_groups: sender_groups.clone(),
    };
    let policy = NavigatorPolicy {
        sibling_floor,
        ..NavigatorPolicy::default()
    };
    println!("policy sibling_floor={sibling_floor}");
    let roster = enrollment::list_users(&pool).await?;
    println!(
        "sender={sender_id} groups={sender_groups:?} roster={:?} top_k={top_k}\n",
        roster.iter().map(|u| &u.user_id).collect::<Vec<_>>()
    );

    for probe in &probes {
        run_probe(
            &pool,
            &tree,
            &sender,
            &sender_id,
            &roster,
            embedder.clone(),
            &policy,
            probe,
            top_k,
        )
        .await?;
    }
    Ok(())
}

/// One probe: today's gate, the flat hits, the fan, the walk, and what
/// serving each resolved third party's card would cost.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one probe's full context, printed as one linear report"
)]
async fn run_probe(
    pool: &sqlx::SqlitePool,
    tree: &wiki::WikiTree,
    sender: &SenderContext,
    sender_id: &str,
    roster: &[enrollment::EnrolledUserLite],
    embedder: Arc<dyn Embedder>,
    policy: &NavigatorPolicy,
    probe: &Probe,
    top_k: usize,
) -> anyhow::Result<()> {
    println!("{}", "=".repeat(78));
    println!("QUERY  {}", probe.phrase);

    // ---- today's gate ------------------------------------------------
    let subjects = recall::turn_subjects(&probe.phrase, sender_id, roster);
    let third_party: Vec<&str> = subjects
        .iter()
        .map(String::as_str)
        .filter(|s| *s != sender_id)
        .collect();
    println!("gate   turn_subjects={subjects:?}   third parties={third_party:?}");

    // ---- what the turn already gets, with no card served --------------
    let hits = recall::wiki_search_unrecorded(
        pool,
        embedder.clone(),
        &probe.phrase,
        top_k,
        fact_index::FactFilters::default(),
        sender,
    )
    .await?;
    println!("flat   {} hits", hits.len());
    for (i, h) in hits.iter().enumerate() {
        let page = h
            .source_path
            .rsplit('/')
            .next()
            .unwrap_or(&h.source_path)
            .to_owned();
        let text = h.text.replace('\n', " ");
        let text = text.chars().take(96).collect::<String>();
        println!("  {:>2}. {:.3}  {:<28} {}", i + 1, h.score, page, text);
    }

    // ---- the founder's own success test -------------------------------
    if !probe.expect.is_empty() {
        let blob: Vec<String> = hits.iter().map(|h| h.text.to_lowercase()).collect();
        for needle in &probe.expect {
            let at = blob.iter().position(|t| t.contains(needle.as_str()));
            match at {
                Some(i) => println!("  EXPECT ✓ \"{needle}\" at rank {}", i + 1),
                None => println!("  EXPECT ✗ \"{needle}\" NOT in the top {top_k}"),
            }
        }
    }

    // ---- and the SECOND half of the pipeline: the walk -----------------
    // The flat hits are only what similarity found. The funnel then opens
    // pages, and their prose can carry the answer the hit list missed —
    // measuring one without the other reads half the turn (founder,
    // 2026-08-04: *«prova a seguire il primo hop, anche questo è
    // importante no?»*).
    let entries = recall_nav::gather_entry_points(
        pool,
        tree,
        sender,
        &[], // no classifier here: topics come from the ingest path
        &hits,
        &[],
    )
    .await?;
    println!("fan    {} entry doors", entries.len());
    for (i, e) in entries.iter().take(8).enumerate() {
        println!(
            "  {:>2}. {:.3} {:<10} {}/{}",
            i + 1,
            e.weight,
            format!("{:?}", e.origin),
            e.wiki_id,
            e.page.display()
        );
    }
    let nav = OpenTheFirst {
        n: policy.pages_per_hop,
    };
    let out = recall_nav::navigate(
        pool,
        tree,
        &nav,
        sender,
        &probe.phrase,
        &entries,
        policy,
        &[],
    )
    .await?;
    println!(
        "walk   {} pages opened over {} hops ({:?})",
        out.fragments.len(),
        out.hops,
        out.stop
    );
    let mut walked = String::new();
    for f in &out.fragments {
        println!(
            "       {}/{}  {} chars",
            f.wiki_id,
            f.page.display(),
            f.text.len()
        );
        walked.push_str(&f.text.to_lowercase());
    }
    if !probe.expect.is_empty() {
        for needle in &probe.expect {
            let flat = hits
                .iter()
                .any(|h| h.text.to_lowercase().contains(needle.as_str()));
            let walk = walked.contains(needle.as_str());
            let verdict = match (flat, walk) {
                (true, _) => "✓ flat",
                (false, true) => "✓ ONLY via the walk",
                (false, false) => "✗ NOWHERE",
            };
            println!("  WHOLE TURN \"{needle}\": {verdict}");
        }
    }

    // ---- what serving each third party's card would cost ---------------
    for s in &third_party {
        let Ok(wid) = mwe_core::types::WikiId::parse(s) else {
            continue;
        };
        let Ok(handle) = tree.locate(&wid) else {
            println!("  card   {s}: no wiki");
            continue;
        };
        let path = handle.abs_dir().join(wiki::PROFILE_FILENAME);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            println!("  card   {s}: no {} on disk", wiki::PROFILE_FILENAME);
            continue;
        };
        // Which of the card's facts the flat slot already carries — the
        // double-pay a loose gate would buy.
        let card_page = format!(
            "wikis/{}/{}",
            handle.rel_dir().to_string_lossy(),
            wiki::PROFILE_FILENAME
        );
        let already: BTreeSet<&str> = hits
            .iter()
            .filter(|h| h.source_path == card_page)
            .map(|h| h.fact_id.as_str())
            .collect();
        println!(
            "  card   {s}: {} chars on disk, {} of its facts already in the flat slot",
            raw.len(),
            already.len()
        );
    }
    println!();
    Ok(())
}
