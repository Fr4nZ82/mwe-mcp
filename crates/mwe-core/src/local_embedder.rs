// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bundled local embedder — Candle backend for bge-m3 (roadmap group 18).
//!
//! ## Why this exists
//!
//! Until now the only [`Embedder`](crate::embedder::Embedder) that shipped was
//! [`OllamaEmbedder`](crate::embedder::OllamaEmbedder), which makes a running
//! Ollama install a hard dependency of *every* deployment — even an
//! all-API one that needs no local generative LLM. This module bundles a
//! local embedder **into the binary** so the default deployment needs nothing
//! external for embeddings.
//!
//! ## Engine choice
//!
//! [Candle](https://github.com/huggingface/candle) — a Rust ML stack whose
//! CPU kernels compile into the binary with no external `onnxruntime` / `.so`
//! at runtime, keeping the self-contained-binary invariant. GPU is an opt-in
//! build feature (roadmap 18f); the shipped default runs on CPU. Caveat
//! surfaced by the 18a spike: candle-core transitively pulls a vendored,
//! statically-linked Oniguruma (`tokenizers[onig]`, a C regex engine) — the
//! runtime binary stays self-contained, but the *build* needs a C compiler.
//!
//! ## Model
//!
//! `bge-m3` is XLM-RoBERTa-large. Its **dense** embedding is the last hidden
//! state of the first token (`<s>` / CLS), L2-normalized — the same CLS
//! pooling llama.cpp (and therefore Ollama) applies, so the vectors point the
//! same way as the existing Ollama-built indexes (validated by the
//! `embedder_spike` example, roadmap 18a).
//!
//! The *engine* lives in the binary; the *weights* are fetched once
//! (roadmap 18c) — `load` takes a directory that already holds
//! `config.json`, `tokenizer.json`, and `pytorch_model.bin`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config as XlmConfig, XLMRobertaModel};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::embedder::{Embedder, EmbedderError, Result};

/// Filename of the model config inside the model directory.
const CONFIG_FILE: &str = "config.json";
/// Filename of the fast-tokenizer spec inside the model directory.
const TOKENIZER_FILE: &str = "tokenizer.json";
/// Filename of the `PyTorch` weights inside the model directory.
///
/// bge-m3 ships only this pickle (`.bin`), no `safetensors`; Candle loads it
/// via [`VarBuilder::from_pth`].
const WEIGHTS_FILE: &str = "pytorch_model.bin";

/// In-process embedder running bge-m3 on Candle.
///
/// Holds the loaded model + tokenizer behind an [`Arc`] so the async
/// [`Embedder::embed`] / [`Embedder::embed_batch`] can hand a cheap clone
/// to [`tokio::task::spawn_blocking`] without borrowing `self`.
pub struct LocalEmbedder {
    inner: Arc<Inner>,
}

/// The loaded, immutable model state shared across blocking tasks.
struct Inner {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
    device: Device,
    dimensions: usize,
    model_id: String,
    /// Token id used to right-pad batches (`<pad>` → 1 for XLM-R).
    pad_id: u32,
}

impl LocalEmbedder {
    /// Load bge-m3 from `model_dir` (must contain `config.json`,
    /// `tokenizer.json`, `pytorch_model.bin`) onto `device`.
    ///
    /// `model_id` is the stable identifier reported by
    /// [`Embedder::model_id`] and used in downstream cache keys / reindex
    /// checks — pass the same value the store was built with (e.g.
    /// `"bge-m3"`) so vectors are not silently mixed across models.
    pub fn load(
        model_dir: &Path,
        device: Device,
        model_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let cfg_path = model_dir.join(CONFIG_FILE);
        let cfg_text = std::fs::read_to_string(&cfg_path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", cfg_path.display()))?;
        let cfg: XlmConfig = serde_json::from_str(&cfg_text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", cfg_path.display()))?;
        let dimensions = cfg.hidden_size;

        let tok_path = model_dir.join(TOKENIZER_FILE);
        let tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer {}: {e}", tok_path.display()))?;

        let weights_path = model_dir.join(WEIGHTS_FILE);
        // `from_pth` reads the pickle state dict; bge-m3's bare `AutoModel`
        // tensor names (`embeddings.*`, `encoder.layer.{i}.*`) line up with
        // the paths `XLMRobertaModel::new` expects at the VarBuilder root.
        let vb = VarBuilder::from_pth(&weights_path, DType::F32, &device)
            .map_err(|e| anyhow::anyhow!("loading weights {}: {e}", weights_path.display()))?;
        let model = XLMRobertaModel::new(&cfg, vb)
            .map_err(|e| anyhow::anyhow!("building xlm-roberta model: {e}"))?;

        // XLM-R's pad token is `<pad>` (id 1); the model derives position
        // ids from this padding id, so right-padding a batch with it plus
        // a zeroed attention mask leaves the real tokens' representations
        // unchanged vs. the unpadded single-text path.
        let pad_id = tokenizer.token_to_id("<pad>").unwrap_or(1);

        Ok(Self {
            inner: Arc::new(Inner {
                model,
                tokenizer,
                device,
                dimensions,
                model_id: model_id.into(),
                pad_id,
            }),
        })
    }
}

impl Inner {
    /// Run the forward pass + CLS pooling + L2 normalize for one text.
    /// Kept separate from the async [`Embedder::embed`] so the heavy,
    /// blocking compute is one self-contained synchronous unit.
    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbedderError::Backend(format!("tokenize: {e}")))?;
        let ids: Vec<u32> = encoding.get_ids().to_vec();
        if ids.is_empty() {
            return Err(EmbedderError::Invalid("text produced no tokens".into()));
        }
        let seq = ids.len();

        let to_backend =
            |e: candle_core::Error, what: &str| EmbedderError::Backend(format!("{what}: {e}"));

        // Single sentence, no padding → attention mask is all ones and
        // token-type ids all zero. Shapes: [1, seq].
        let input_ids = Tensor::new(ids.as_slice(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| to_backend(e, "input_ids tensor"))?;
        let attention_mask = Tensor::ones((1, seq), DType::U32, &self.device)
            .map_err(|e| to_backend(e, "attention_mask tensor"))?;
        let token_type_ids = Tensor::zeros((1, seq), DType::U32, &self.device)
            .map_err(|e| to_backend(e, "token_type_ids tensor"))?;

        let hidden = self
            .model
            .forward(
                &input_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )
            .map_err(|e| to_backend(e, "forward"))?;

        // CLS pooling: hidden is [1, seq, hidden]; take token 0 → [hidden].
        let cls = hidden.i((0, 0)).map_err(|e| to_backend(e, "cls slice"))?;
        let mut vector: Vec<f32> = cls.to_vec1().map_err(|e| to_backend(e, "to_vec"))?;
        if vector.len() != self.dimensions {
            return Err(EmbedderError::Protocol(format!(
                "expected {}-dim vector, got {}",
                self.dimensions,
                vector.len()
            )));
        }
        l2_normalize(&mut vector);
        Ok(vector)
    }

    /// Embed a batch in one padded forward pass. Right-pads every row to
    /// the longest with `pad_id` and masks the padding out, so each row's
    /// CLS vector matches its single-text [`Self::embed_blocking`] result.
    fn embed_batch_blocking(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut rows: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        for t in texts {
            if t.is_empty() {
                return Err(EmbedderError::Invalid("empty text".into()));
            }
            let encoding = self
                .tokenizer
                .encode(t.as_str(), true)
                .map_err(|e| EmbedderError::Backend(format!("tokenize: {e}")))?;
            let ids = encoding.get_ids().to_vec();
            if ids.is_empty() {
                return Err(EmbedderError::Invalid("text produced no tokens".into()));
            }
            rows.push(ids);
        }
        let batch = rows.len();
        let (ids_flat, mask_flat, max_len) = pad_rows(&rows, self.pad_id);

        let to_backend =
            |e: candle_core::Error, what: &str| EmbedderError::Backend(format!("{what}: {e}"));

        // Shapes: [batch, max_len]. Padding tokens are masked out, so the
        // CLS row of each sequence is unaffected by its neighbours.
        let input_ids = Tensor::from_vec(ids_flat, (batch, max_len), &self.device)
            .map_err(|e| to_backend(e, "input_ids tensor"))?;
        let attention_mask = Tensor::from_vec(mask_flat, (batch, max_len), &self.device)
            .map_err(|e| to_backend(e, "attention_mask tensor"))?;
        let token_type_ids = Tensor::zeros((batch, max_len), DType::U32, &self.device)
            .map_err(|e| to_backend(e, "token_type_ids tensor"))?;

        let hidden = self
            .model
            .forward(
                &input_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )
            .map_err(|e| to_backend(e, "forward"))?;

        let mut out = Vec::with_capacity(batch);
        for i in 0..batch {
            // CLS pooling: row i, token 0 → [hidden].
            let cls = hidden.i((i, 0)).map_err(|e| to_backend(e, "cls slice"))?;
            let mut vector: Vec<f32> = cls.to_vec1().map_err(|e| to_backend(e, "to_vec"))?;
            if vector.len() != self.dimensions {
                return Err(EmbedderError::Protocol(format!(
                    "expected {}-dim vector, got {}",
                    self.dimensions,
                    vector.len()
                )));
            }
            l2_normalize(&mut vector);
            out.push(vector);
        }
        Ok(out)
    }
}

/// Right-pad token-id rows to the longest, returning the flattened
/// `[batch * max_len]` ids + attention mask (`1` = real, `0` = pad) and the
/// padded length. Pure (no model) so it is unit-testable.
fn pad_rows(rows: &[Vec<u32>], pad_id: u32) -> (Vec<u32>, Vec<u32>, usize) {
    let max_len = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut ids = Vec::with_capacity(rows.len() * max_len);
    let mut mask = Vec::with_capacity(rows.len() * max_len);
    for row in rows {
        for &id in row {
            ids.push(id);
            mask.push(1u32);
        }
        for _ in row.len()..max_len {
            ids.push(pad_id);
            mask.push(0u32);
        }
    }
    (ids, mask, max_len)
}

/// L2-normalize a vector in place. A zero vector is left untouched (no NaNs).
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[async_trait::async_trait]
impl Embedder for LocalEmbedder {
    fn model_id(&self) -> &str {
        &self.inner.model_id
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::Invalid("empty text".into()));
        }
        // Offload the blocking forward to the blocking pool so the async
        // worker thread is never stalled by the (tens-to-hundreds-of-ms)
        // CPU compute.
        let inner = Arc::clone(&self.inner);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || inner.embed_blocking(&text))
            .await
            .map_err(|e| EmbedderError::Backend(format!("embed task join: {e}")))?
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inner = Arc::clone(&self.inner);
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || inner.embed_batch_blocking(&texts))
            .await
            .map_err(|e| EmbedderError::Backend(format!("embed_batch task join: {e}")))?
    }
}

// ---------- Weight distribution (roadmap 18c) ----------

/// `HuggingFace` base URL for the bge-m3 weights (`resolve/main` serves the
/// raw LFS blobs over HTTPS).
const HF_BGE_M3_BASE: &str = "https://huggingface.co/BAAI/bge-m3/resolve/main";

/// One downloadable weight file + its pinned SHA-256, verified right after
/// a fresh download so a corrupted or substituted blob fails loud.
struct WeightFile {
    name: &'static str,
    sha256: &'static str,
}

/// The three files [`LocalEmbedder::load`] needs, checksums pinned to the
/// released bge-m3 revision served by `HuggingFace`.
const BGE_M3_FILES: &[WeightFile] = &[
    WeightFile {
        name: CONFIG_FILE,
        sha256: "26159e7ad065073448460117eb24b7a4572f6f4e78eadff65dc0a11c052449fa",
    },
    WeightFile {
        name: TOKENIZER_FILE,
        sha256: "21106b6d7dab2952c1d496fb21d5dc9db75c28ed361a05f5020bbba27810dd08",
    },
    WeightFile {
        name: WEIGHTS_FILE,
        sha256: "b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38",
    },
];

/// Default on-disk cache for auto-downloaded weights.
///
/// `$XDG_CACHE_HOME/mwe-mcp/models/<model_id>`, falling back to
/// `$HOME/.cache/...`, then a relative `.cache/...`. The *engine* lives in
/// the binary; the *weights* live here, fetched once (roadmap 18c).
#[must_use]
pub fn default_cache_dir(model_id: &str) -> PathBuf {
    resolve_cache_dir(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        model_id,
    )
}

/// Pure cache-dir resolution, split out so it is unit-testable without
/// touching the process environment. A relative `XDG_CACHE_HOME` is
/// invalid per the XDG spec, so it is ignored in favour of `$HOME/.cache`.
fn resolve_cache_dir(
    xdg_cache_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    model_id: &str,
) -> PathBuf {
    let base = xdg_cache_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("mwe-mcp").join("models").join(model_id)
}

/// Ensure the bge-m3 weights are present in `dir`, downloading any missing
/// file from `HuggingFace` over rustls and verifying its pinned SHA-256.
///
/// Idempotent: a file that already exists (non-empty) is trusted and
/// skipped — the checksum is verified only on a fresh download, so a warm
/// cache never pays a multi-GB rehash at boot. The streamed download lands
/// in a `.part` sibling and is renamed into place atomically, so an
/// interrupted fetch never leaves a half-file masquerading as complete.
///
/// # Errors
///
/// Network / IO failures, a non-success HTTP status, or a checksum mismatch
/// (the offending file is removed before returning).
pub async fn ensure_bge_m3_weights(dir: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60 * 60))
        .build()?;
    for wf in BGE_M3_FILES {
        let dest = dir.join(wf.name);
        if tokio::fs::try_exists(&dest).await.unwrap_or(false)
            && tokio::fs::metadata(&dest).await?.len() > 0
        {
            continue;
        }
        let url = format!("{HF_BGE_M3_BASE}/{}", wf.name);
        tracing::info!(file = wf.name, %url, "bundled embedder: fetching weight");
        download_to(&client, &url, &dest).await?;
        let got = sha256_file(&dest)?;
        if got != wf.sha256 {
            let _ = std::fs::remove_file(&dest);
            anyhow::bail!(
                "checksum mismatch for {} (expected {}, got {got})",
                wf.name,
                wf.sha256
            );
        }
        tracing::info!(file = wf.name, "bundled embedder: weight verified");
    }
    Ok(())
}

/// Stream a URL to `dest` via a `.part` temp + atomic rename (the weights
/// file is ~2.3 GB, so the body is never buffered whole in memory).
async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut resp = client.get(url).send().await?.error_for_status()?;
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// SHA-256 of a file, streamed in 1 MiB chunks (the weights file is GBs).
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_rows_right_pads_with_mask() {
        let rows = vec![vec![10u32, 11, 12], vec![20u32]];
        let (ids, mask, max_len) = pad_rows(&rows, 1);
        assert_eq!(max_len, 3);
        assert_eq!(ids, vec![10, 11, 12, 20, 1, 1]);
        assert_eq!(mask, vec![1, 1, 1, 1, 0, 0]);
    }

    #[test]
    fn pad_rows_empty_is_empty() {
        let (ids, mask, max_len) = pad_rows(&[], 1);
        assert!(ids.is_empty());
        assert!(mask.is_empty());
        assert_eq!(max_len, 0);
    }

    #[test]
    fn l2_normalize_unit_length_and_zero_safe() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        let norm = v[0].hypot(v[1]);
        assert!((norm - 1.0).abs() < 1e-6, "norm = {norm}");
        let mut z = vec![0.0f32, 0.0, 0.0];
        l2_normalize(&mut z);
        assert_eq!(
            z,
            vec![0.0, 0.0, 0.0],
            "zero vector must stay zero (no NaN)"
        );
    }

    #[test]
    fn resolve_cache_dir_prefers_absolute_xdg() {
        let dir = resolve_cache_dir(
            Some(std::ffi::OsStr::new("/var/cache")),
            Some(std::ffi::OsStr::new("/home/u")),
            "bge-m3",
        );
        assert_eq!(dir, PathBuf::from("/var/cache/mwe-mcp/models/bge-m3"));
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_home_cache() {
        let dir = resolve_cache_dir(None, Some(std::ffi::OsStr::new("/home/u")), "bge-m3");
        assert_eq!(dir, PathBuf::from("/home/u/.cache/mwe-mcp/models/bge-m3"));
    }

    #[test]
    fn resolve_cache_dir_ignores_relative_xdg() {
        // A relative XDG_CACHE_HOME is invalid per spec → fall back to HOME.
        let dir = resolve_cache_dir(
            Some(std::ffi::OsStr::new("relative/cache")),
            Some(std::ffi::OsStr::new("/home/u")),
            "bge-m3",
        );
        assert_eq!(dir, PathBuf::from("/home/u/.cache/mwe-mcp/models/bge-m3"));
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().expect("temp");
        f.write_all(b"abc").expect("write");
        // SHA-256("abc")
        assert_eq!(
            sha256_file(f.path()).expect("hash"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Validates the pinned checksums against a real cached model dir, so a
    /// stale constant is caught without a 2.3 GB download. Gated on
    /// `MWE_TEST_BGE_M3_DIR`.
    #[test]
    fn pinned_checksums_match_cached_weights() {
        let Ok(dir) = std::env::var("MWE_TEST_BGE_M3_DIR") else {
            eprintln!("skipping pinned_checksums: set MWE_TEST_BGE_M3_DIR");
            return;
        };
        let dir = PathBuf::from(dir);
        for wf in BGE_M3_FILES {
            let got = sha256_file(&dir.join(wf.name)).expect("hash cached file");
            assert_eq!(got, wf.sha256, "pinned checksum drift for {}", wf.name);
        }
    }

    /// End-to-end parity test, gated on a local bge-m3 model directory
    /// (the weights are ~2.2 GB — not in CI). Set `MWE_TEST_BGE_M3_DIR` to
    /// the dir holding `config.json` / `tokenizer.json` /
    /// `pytorch_model.bin` to run it; otherwise it skips.
    #[tokio::test]
    async fn batch_matches_single_when_model_available() {
        let Ok(dir) = std::env::var("MWE_TEST_BGE_M3_DIR") else {
            eprintln!(
                "skipping batch_matches_single: set MWE_TEST_BGE_M3_DIR to a bge-m3 model dir"
            );
            return;
        };
        let e = LocalEmbedder::load(std::path::Path::new(&dir), Device::Cpu, "bge-m3")
            .expect("load bge-m3");
        let texts = vec![
            "il gatto dorme sul tappeto".to_string(),
            "quantum chromodynamics describes the strong interaction".to_string(),
        ];
        let single0 = e.embed(&texts[0]).await.expect("embed 0");
        let single1 = e.embed(&texts[1]).await.expect("embed 1");
        let batch = e.embed_batch(&texts).await.expect("embed_batch");

        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].len(), e.dimensions());
        // Vectors are L2-normalized, so the dot product is the cosine.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(
            cos(&batch[0], &single0) > 0.999,
            "batch[0] vs single0 cos = {}",
            cos(&batch[0], &single0)
        );
        assert!(
            cos(&batch[1], &single1) > 0.999,
            "batch[1] vs single1 cos = {}",
            cos(&batch[1], &single1)
        );
        // Empty batch short-circuits without touching the model.
        assert!(e.embed_batch(&[]).await.expect("empty batch").is_empty());
    }
}
