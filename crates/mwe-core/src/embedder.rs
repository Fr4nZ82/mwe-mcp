// SPDX-License-Identifier: AGPL-3.0-or-later
//! Embedding adapter — trait + Ollama backend.
//!
//! ## Why a trait
//!
//! The recall pipeline ([`crate::recall`]) needs vectors of
//! `f32` for similarity search. Which backend produces them — local
//! Ollama, an OpenAI-compatible HTTP API, a future bge-m3 binding —
//! is a deployment-time choice the operator makes in
//! `mwe-mcp.config.yaml`. The trait pins the *contract* (text → vector
//! of known dimension) and leaves the choice of backend to a
//! `Box<dyn Embedder>` stored in [`crate::config`].
//!
//! ## What ships
//!
//! - The [`Embedder`] trait itself (async, `Send + Sync`, dyn-safe via
//!   `async_trait`).
//! - [`OllamaEmbedder`] — an HTTP client for the Ollama embedding API
//!   (`POST /api/embeddings`) backed by `reqwest` with `rustls-tls`.
//!   Defaults to the `bge-m3` model (the multilingual stack baseline).
//! - A [`FakeEmbedder`] under `#[cfg(any(test, feature = "test-fakes"))]`
//!   so downstream tests can build a recall pipeline without hitting
//!   the network. (It is kept test-only; the
//!   `test-fakes` feature is reserved for tests in other
//!   crates.)
//!
//! Provider-specific tuning (timeouts, retries, batching, auth) lives
//! on each backend implementation, not in the trait.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Default Ollama base URL — the conventional `http://localhost:11434`
/// for a same-host install. Overridable in `OllamaEmbedder::new`.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Default embedding model. `bge-m3` is the multilingual baseline
/// for the embedding stack.
pub const DEFAULT_EMBED_MODEL: &str = "bge-m3";

/// HTTP request/response timeout for embedding calls.
///
/// Embedding requests are CPU-bound on the Ollama side and usually
/// finish in well under a second; a 30 s ceiling catches a hung
/// backend without blocking the recall path forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors raised by an [`Embedder`].
#[derive(Debug, Error)]
pub enum EmbedderError {
    /// Backend rejected the input (e.g. text too long, unknown model).
    /// Caller should not retry without changing the request.
    #[error("embedder invalid request: {0}")]
    Invalid(String),
    /// Transport-level failure (DNS, TCP, TLS, timeout). Retriable.
    #[error("embedder transport error: {0}")]
    Transport(String),
    /// Backend returned a 5xx or otherwise malformed response.
    #[error("embedder backend error: {0}")]
    Backend(String),
    /// Internal sanity check failure (e.g. backend returned a vector
    /// of unexpected dimension). Should not happen in practice;
    /// surfaced as `Backend`-equivalent severity.
    #[error("embedder protocol error: {0}")]
    Protocol(String),
}

/// Result alias for embedder operations.
pub type Result<T> = std::result::Result<T, EmbedderError>;

/// Contract every embedding backend honours.
///
/// Implementations must be `Send + Sync` because the recall pipeline
/// stores them behind `Arc<dyn Embedder>` and calls them from any
/// tokio task.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Stable identifier of the *model* this embedder produces vectors
    /// for. Used as part of cache keys + sanity checks so that swapping
    /// models invalidates downstream caches automatically.
    fn model_id(&self) -> &str;

    /// Vector dimension every successful call returns. The recall
    /// pipeline preallocates buffers from this, and asserts on every
    /// returned vector for sanity.
    fn dimensions(&self) -> usize;

    /// Embed a single piece of text and return its vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of texts. The default implementation calls
    /// [`embed`] in a loop; backends that support native batching
    /// (Ollama does not at the time of writing) override this for
    /// better throughput.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// HTTP client for the Ollama embedding API.
///
/// Posts to `{base_url}/api/embeddings` with a body of the shape
/// `{ "model": "...", "prompt": "..." }` and expects
/// `{ "embedding": [<floats>] }` back. Anything else is surfaced as
/// [`EmbedderError::Backend`] or [`EmbedderError::Protocol`].
pub struct OllamaEmbedder {
    client: Client,
    base_url: String,
    model: String,
    dimensions: usize,
}

#[derive(Debug, Serialize)]
struct OllamaEmbeddingRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingResponse {
    embedding: Vec<f32>,
}

impl OllamaEmbedder {
    /// Build a fresh embedder. `base_url` should not include a path
    /// suffix (we add `/api/embeddings`). `dimensions` is the size of
    /// the vectors the chosen model produces — for `bge-m3` that is
    /// 1024.
    ///
    /// `dimensions` is a static value the operator knows from the
    /// model's documentation; we keep it explicit instead of probing
    /// the server to avoid a startup-time round trip.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| EmbedderError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            model: model.into(),
            dimensions,
        })
    }

    /// Shortcut for "Ollama on localhost with `bge-m3` (1024-dim)".
    pub fn local_bge_m3() -> Result<Self> {
        Self::new(DEFAULT_OLLAMA_URL, DEFAULT_EMBED_MODEL, 1024)
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::Invalid("empty text".into()));
        }
        let url = format!("{}/api/embeddings", self.base_url.trim_end_matches('/'));
        let body = OllamaEmbeddingRequest {
            model: &self.model,
            prompt: text,
        };
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbedderError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(EmbedderError::Backend(format!(
                "HTTP {status}: {}",
                body.chars().take(500).collect::<String>()
            )));
        }
        let parsed: OllamaEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| EmbedderError::Protocol(format!("decoding response: {e}")))?;
        if parsed.embedding.len() != self.dimensions {
            return Err(EmbedderError::Protocol(format!(
                "expected {}-dim vector, got {} dims",
                self.dimensions,
                parsed.embedding.len()
            )));
        }
        Ok(parsed.embedding)
    }
}

/// Deterministic in-process embedder used by tests.
///
/// Returns a vector seeded by the input text (so the same text gets
/// the same vector and the recall layer can be exercised end-to-end
/// without HTTP). Output dimension is configurable so tests can mirror
/// the production model size.
#[cfg(any(test, feature = "test-fakes"))]
pub struct FakeEmbedder {
    model: String,
    dimensions: usize,
    /// When set, `embed` always returns this exact vector. Used by
    /// recall-pipeline tests that need the query embedding to match a
    /// pre-stored row embedding for a deterministic cosine.
    fixed: Option<Vec<f32>>,
}

#[cfg(any(test, feature = "test-fakes"))]
impl FakeEmbedder {
    /// Build a fake with the given model id + dimensions, using the
    /// default hash-derived deterministic embedding.
    pub fn new(model: impl Into<String>, dimensions: usize) -> Self {
        Self {
            model: model.into(),
            dimensions,
            fixed: None,
        }
    }

    /// Build a fake that always returns `embedding` regardless of the
    /// input text. Lets a test plant a vector in the DB and then issue
    /// a query whose embedding matches it exactly — useful for testing
    /// the cosine + ACL filter without depending on the hash-derived
    /// path.
    pub fn with_fixed_embedding(model: impl Into<String>, embedding: Vec<f32>) -> Self {
        let dimensions = embedding.len();
        Self {
            model: model.into(),
            dimensions,
            fixed: Some(embedding),
        }
    }
}

#[cfg(any(test, feature = "test-fakes"))]
#[async_trait]
impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Hash-derived deterministic vector. Not a real embedding —
        // just stable enough for the test layer.
        use sha2::{Digest, Sha256};

        if text.is_empty() {
            return Err(EmbedderError::Invalid("empty text".into()));
        }
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        let mut out = Vec::with_capacity(self.dimensions);
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let digest = hasher.finalize();
        let bytes: &[u8] = digest.as_slice();
        for i in 0..self.dimensions {
            let b = bytes[i % bytes.len()];
            // Map byte to f32 in [-1.0, 1.0] so the resemblance to a
            // real unit-ish vector is at least directionally sane.
            out.push((f32::from(b) - 127.5) / 127.5);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fake_embedder_is_deterministic_and_dimensioned() {
        let e = FakeEmbedder::new("fake-256", 256);
        assert_eq!(e.model_id(), "fake-256");
        assert_eq!(e.dimensions(), 256);
        let a = e.embed("ciao mondo").await.expect("embed");
        let b = e.embed("ciao mondo").await.expect("embed");
        assert_eq!(a, b, "same text must produce same vector");
        assert_eq!(a.len(), 256);
        let c = e.embed("altro testo").await.expect("embed");
        assert_ne!(a, c, "different text must produce different vector");
    }

    #[tokio::test]
    async fn fake_embedder_rejects_empty_text() {
        let e = FakeEmbedder::new("fake-256", 256);
        let err = e.embed("").await.expect_err("must reject");
        assert!(matches!(err, EmbedderError::Invalid(_)));
    }

    #[tokio::test]
    async fn ollama_embedder_posts_to_api_and_decodes_response() {
        let server = MockServer::start().await;
        let dims = 4;
        let response = serde_json::json!({ "embedding": [0.1f32, -0.2, 0.3, 0.4] });
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(body_json(serde_json::json!({
                "model": "test-model",
                "prompt": "embed me",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let embedder = OllamaEmbedder::new(server.uri(), "test-model", dims).expect("new");
        let v = embedder.embed("embed me").await.expect("embed");
        assert_eq!(v.len(), dims);
        assert!((v[0] - 0.1).abs() < 1e-6);
    }

    #[tokio::test]
    async fn ollama_embedder_surfaces_http_errors_as_backend() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let embedder = OllamaEmbedder::new(server.uri(), "x", 4).expect("new");
        let err = embedder.embed("anything").await.expect_err("must fail");
        assert!(matches!(err, EmbedderError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn ollama_embedder_detects_dim_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "embedding": [0.1f32, 0.2] })),
            )
            .mount(&server)
            .await;

        // Expect 4 dims but server returns 2.
        let embedder = OllamaEmbedder::new(server.uri(), "x", 4).expect("new");
        let err = embedder.embed("anything").await.expect_err("must fail");
        assert!(matches!(err, EmbedderError::Protocol(_)), "{err:?}");
    }

    #[tokio::test]
    async fn embedder_default_batch_calls_embed_per_item() {
        let e = FakeEmbedder::new("fake-8", 8);
        let texts = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let vs = e.embed_batch(&texts).await.expect("batch");
        assert_eq!(vs.len(), 3);
        assert!(vs.iter().all(|v| v.len() == 8));
    }
}
