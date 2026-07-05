// SPDX-License-Identifier: AGPL-3.0-or-later
//! Engine spike for the bundled embedder — roadmap 18a (the gate).
//!
//! Validates that bge-m3 runs in Candle on CPU and produces vectors that
//! match the Ollama bge-m3 baseline closely enough to reuse existing
//! indexes. Downloads the weights once (into `./work/models/bge-m3` by
//! default, gitignored), embeds a multilingual sentence set through both
//! Candle and Ollama, and reports per-sentence cosine + Candle latency.
//!
//! ```text
//! cargo run -p mwe-core --example embedder_spike --features local-embedder --release
//!
//! MWE_SPIKE_MODEL_DIR   where to cache/read the weights (default ./work/models/bge-m3)
//! MWE_SPIKE_OLLAMA_URL  Ollama base URL (default http://localhost:11434)
//! ```
//!
//! Gate: PASS when the minimum cosine(Candle, Ollama) across the set is at
//! least 0.98 (same direction, so indexes are reusable). Lower means a
//! reindex (roadmap 18g) or a pooling/normalization mismatch to investigate.

// Throwaway measurement harness: the means below cast tiny element counts
// (single digits) to f32, where the precision-loss lint is pure noise.
#![allow(clippy::cast_precision_loss)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use candle_core::Device;
use mwe_core::embedder::Embedder;
use mwe_core::local_embedder::LocalEmbedder;

const HF_BASE: &str = "https://huggingface.co/BAAI/bge-m3/resolve/main";
const FILES: &[&str] = &["config.json", "tokenizer.json", "pytorch_model.bin"];
const GATE_THRESHOLD: f32 = 0.98;

/// Sentences chosen to stress multilingual coverage and to include a few
/// near-paraphrase pairs (so we can eyeball that semantic structure
/// survives, not just the per-vector match).
const SENTENCES: &[&str] = &[
    "The cat sat quietly on the warm windowsill.",
    "A feline rested calmly by the sunny window.", // paraphrase of #0
    "Il gatto dormiva sul davanzale caldo.",
    "Quantum entanglement links two particles across any distance.",
    "L'intreccio quantistico collega due particelle a qualsiasi distanza.", // it of #3
    "我喜欢在早晨喝一杯热咖啡。",
    "Ich trinke jeden Morgen gerne eine Tasse heißen Kaffee.",
    "The quarterly revenue report is due next Friday afternoon.",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model_dir: PathBuf = std::env::var("MWE_SPIKE_MODEL_DIR")
        .unwrap_or_else(|_| "./work/models/bge-m3".to_string())
        .into();
    let ollama_url = std::env::var("MWE_SPIKE_OLLAMA_URL")
        .unwrap_or_else(|_| "http://localhost:11434".to_string());

    println!("== mwe-mcp bundled-embedder spike (18a) ==");
    println!("model dir : {}", model_dir.display());
    println!("ollama    : {ollama_url}\n");

    ensure_weights(&model_dir).await?;

    println!("loading bge-m3 into Candle (CPU)…");
    let load_start = Instant::now();
    let embedder = LocalEmbedder::load(&model_dir, Device::Cpu, "bge-m3")?;
    println!(
        "loaded in {:.1}s — {} dims\n",
        load_start.elapsed().as_secs_f32(),
        embedder.dimensions()
    );

    let client = reqwest::Client::new();
    let mut cosines = Vec::new();
    let mut latencies_ms = Vec::new();
    let mut candle_vecs = Vec::new();

    println!("{:<6} {:>10} {:>12}   text", "idx", "cos(o)", "candle(ms)");
    println!("{}", "-".repeat(72));
    for (i, &text) in SENTENCES.iter().enumerate() {
        let t0 = Instant::now();
        let cv = embedder.embed(text).await?;
        let ms = t0.elapsed().as_secs_f32() * 1000.0;

        let ov = ollama_embed(&client, &ollama_url, text).await?;
        let cos = cosine(&cv, &ov);

        cosines.push(cos);
        latencies_ms.push(ms);
        candle_vecs.push(cv);

        let preview: String = text.chars().take(38).collect();
        println!("{i:<6} {cos:>10.4} {ms:>12.1}   {preview}");
    }

    // Candle-internal sanity: the two paraphrase pairs (0/1 same language,
    // 3/4 cross-language) should be markedly closer than an unrelated pair.
    println!("\ncandle-internal cosine (semantic structure sanity):");
    println!(
        "  #0 vs #1 (en paraphrase)   : {:.4}",
        cosine(&candle_vecs[0], &candle_vecs[1])
    );
    println!(
        "  #3 vs #4 (en/it same idea) : {:.4}",
        cosine(&candle_vecs[3], &candle_vecs[4])
    );
    println!(
        "  #0 vs #7 (unrelated)       : {:.4}",
        cosine(&candle_vecs[0], &candle_vecs[7])
    );

    let min_cos = cosines.iter().copied().fold(f32::INFINITY, f32::min);
    let mean_cos = cosines.iter().sum::<f32>() / cosines.len() as f32;
    let mean_ms = latencies_ms.iter().sum::<f32>() / latencies_ms.len() as f32;
    let max_ms = latencies_ms.iter().copied().fold(0.0f32, f32::max);

    println!("\n== summary ==");
    println!("cosine vs ollama : min {min_cos:.4}  mean {mean_cos:.4}");
    println!("candle latency   : mean {mean_ms:.1}ms  max {max_ms:.1}ms");

    if min_cos >= GATE_THRESHOLD {
        println!(
            "\nGATE PASS ✅  (min cosine {min_cos:.4} >= {GATE_THRESHOLD}) — vectors align with the Ollama baseline; indexes reusable."
        );
    } else {
        println!(
            "\nGATE REVIEW ⚠️  (min cosine {min_cos:.4} < {GATE_THRESHOLD}) — investigate pooling/normalization, or plan a one-time reindex (18g)."
        );
    }
    Ok(())
}

/// Download the three model files into `dir` if any is missing.
async fn ensure_weights(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60 * 30))
        .build()?;
    for &file in FILES {
        let dest = dir.join(file);
        if dest.exists() && std::fs::metadata(&dest)?.len() > 0 {
            println!("have   {file}");
            continue;
        }
        let url = format!("{HF_BASE}/{file}");
        println!("fetch  {file}  <- {url}");
        download_to(&client, &url, &dest).await?;
    }
    println!();
    Ok(())
}

/// Stream a URL to a file, chunk by chunk (the weights are ~2.3 GB).
async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut resp = client.get(url).send().await?.error_for_status()?;
    let total = resp.content_length();
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut written: u64 = 0;
    let mut last_logged: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if written - last_logged >= 200 * 1024 * 1024 {
            last_logged = written;
            match total {
                Some(t) => println!("       … {} / {} MiB", written >> 20, t >> 20),
                None => println!("       … {} MiB", written >> 20),
            }
        }
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// One embedding from Ollama's `/api/embeddings` (the existing baseline).
async fn ollama_embed(
    client: &reqwest::Client,
    base_url: &str,
    text: &str,
) -> anyhow::Result<Vec<f32>> {
    #[derive(serde::Serialize)]
    struct Req<'a> {
        model: &'a str,
        prompt: &'a str,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        embedding: Vec<f32>,
    }
    let url = format!("{}/api/embeddings", base_url.trim_end_matches('/'));
    let resp: Resp = client
        .post(url)
        .json(&Req {
            model: "bge-m3",
            prompt: text,
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.embedding)
}

/// Cosine similarity (normalizes both, so raw vs L2-normalized inputs match).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}
