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
//! - [`default_cache_dir`] — where the bundled backend keeps its
//!   downloaded weights. The backend itself is behind the
//!   `local-embedder` feature; this is plain path arithmetic and lives
//!   here so every build compiles it and every platform's test run
//!   checks it.
//!
//! Provider-specific tuning (timeouts, retries, batching, auth) lives
//! on each backend implementation, not in the trait.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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

/// Which per-user cache root the platform names, and what the resolver
/// therefore reads from the environment.
///
/// The three supported platforms fall into two conventions, and the split
/// is a parameter rather than a `cfg!` so the resolver can be tested for
/// all of them from one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheHost {
    /// Linux and macOS: `$XDG_CACHE_HOME`, else `$HOME/.cache`.
    Unix,
    /// Windows: `%XDG_CACHE_HOME%` (the packaged scheduled task sets it
    /// machine-wide, so the daemon's weights land inside its own tree),
    /// else `%LOCALAPPDATA%` — the per-user store Windows itself names
    /// for exactly this kind of large, re-downloadable data.
    Windows,
}

/// Default on-disk cache for auto-downloaded weights.
///
/// `<per-user cache root>/mwe-mcp/models/<model_id>`, where the root is
/// `$XDG_CACHE_HOME` when it is set to an absolute path, then
/// `%LOCALAPPDATA%` on Windows and `$HOME/.cache` elsewhere. The *engine*
/// lives in the binary; the *weights* live here, fetched once — 2.2 GB of
/// them, which is why the directory has to be the same one on every boot.
#[must_use]
pub fn default_cache_dir(model_id: &str) -> PathBuf {
    let host = if cfg!(windows) {
        CacheHost::Windows
    } else {
        CacheHost::Unix
    };
    resolve_cache_dir(
        host,
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        model_id,
    )
}

/// Pure cache-dir resolution, split out so it is unit-testable without
/// touching the process environment.
///
/// A root that is not absolute is ignored (a relative `XDG_CACHE_HOME` is
/// invalid per the XDG spec, and a relative `%LOCALAPPDATA%` is nonsense):
/// it would put the weights wherever the process happened to be started,
/// which is a different directory per launch and a 2.2 GB re-download each
/// time. The last resort is a relative `.cache` — reached only when the
/// platform's own variable is missing.
fn resolve_cache_dir(
    host: CacheHost,
    xdg_cache_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    local_app_data: Option<&std::ffi::OsStr>,
    model_id: &str,
) -> PathBuf {
    let absolute = |v: Option<&std::ffi::OsStr>| {
        v.map(PathBuf::from)
            .filter(|p| is_absolute_for(host, p.as_path()))
    };
    let base = absolute(xdg_cache_home)
        .or_else(|| match host {
            CacheHost::Unix => absolute(home).map(|h| h.join(".cache")),
            CacheHost::Windows => absolute(local_app_data),
        })
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("mwe-mcp").join("models").join(model_id)
}

/// Is `path` absolute *for `host`*, decided by the platform being resolved
/// for rather than by the one this binary was compiled on — which is what
/// lets one machine check the answer for all three.
fn is_absolute_for(host: CacheHost, path: &Path) -> bool {
    let s = path.to_string_lossy();
    match host {
        CacheHost::Unix => s.starts_with('/'),
        // A drive-qualified path (`C:\…`, `C:/…`) or a UNC share
        // (`\\host\share\…`). A bare `\dir` is root-relative to whichever
        // drive the process sits on, so it is not a stable place either.
        CacheHost::Windows => {
            s.starts_with(r"\\")
                || matches!(s.as_bytes(),
                    [drive, b':', sep, ..]
                        if drive.is_ascii_alphabetic() && (*sep == b'\\' || *sep == b'/'))
        },
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

    /// The weights suffix under whichever root was chosen, built the way
    /// the resolver builds it so the assertions name the **root** — the
    /// only thing the platform split decides — and not the separator of
    /// the machine running the test.
    fn under(root: &str) -> PathBuf {
        PathBuf::from(root)
            .join("mwe-mcp")
            .join("models")
            .join("bge-m3")
    }

    #[test]
    fn resolve_cache_dir_prefers_absolute_xdg() {
        let dir = resolve_cache_dir(
            CacheHost::Unix,
            Some(std::ffi::OsStr::new("/var/cache")),
            Some(std::ffi::OsStr::new("/home/u")),
            None,
            "bge-m3",
        );
        assert_eq!(dir, under("/var/cache"));
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_home_cache_on_linux() {
        let dir = resolve_cache_dir(
            CacheHost::Unix,
            None,
            Some(std::ffi::OsStr::new("/home/u")),
            None,
            "bge-m3",
        );
        assert_eq!(dir, under("/home/u/.cache"));
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_home_cache_on_macos() {
        let dir = resolve_cache_dir(
            CacheHost::Unix,
            None,
            Some(std::ffi::OsStr::new("/Users/u")),
            None,
            "bge-m3",
        );
        assert_eq!(dir, under("/Users/u/.cache"));
    }

    /// Windows has no `$HOME`: without this branch the weights landed in a
    /// `.cache` beside whatever directory the process was started from, so
    /// a service started elsewhere re-downloaded 2.2 GB. `%LOCALAPPDATA%`
    /// is the per-user root, and it is the same one on every boot.
    #[test]
    fn resolve_cache_dir_uses_local_app_data_on_windows() {
        let dir = resolve_cache_dir(
            CacheHost::Windows,
            None,
            None,
            Some(std::ffi::OsStr::new(r"C:\Users\u\AppData\Local")),
            "bge-m3",
        );
        assert_eq!(dir, under(r"C:\Users\u\AppData\Local"));
        assert_ne!(dir, under(".cache"), "not a directory that moves");
    }

    /// The packaged scheduled task pins `XDG_CACHE_HOME` inside the
    /// daemon's own tree, so it wins on Windows as well.
    #[test]
    fn resolve_cache_dir_prefers_absolute_xdg_on_windows() {
        let dir = resolve_cache_dir(
            CacheHost::Windows,
            Some(std::ffi::OsStr::new(r"C:\ProgramData\mwe-mcp\cache")),
            None,
            Some(std::ffi::OsStr::new(r"C:\Users\u\AppData\Local")),
            "bge-m3",
        );
        assert_eq!(dir, under(r"C:\ProgramData\mwe-mcp\cache"));
    }

    #[test]
    fn resolve_cache_dir_ignores_relative_xdg() {
        // A relative XDG_CACHE_HOME is invalid per spec → fall back to HOME.
        let dir = resolve_cache_dir(
            CacheHost::Unix,
            Some(std::ffi::OsStr::new("relative/cache")),
            Some(std::ffi::OsStr::new("/home/u")),
            None,
            "bge-m3",
        );
        assert_eq!(dir, under("/home/u/.cache"));
    }

    /// A drive-relative `\dir` follows the process's current drive, and a
    /// bare `cache` its current directory: neither is a stable place, so
    /// both are refused the same way a relative `XDG_CACHE_HOME` is.
    #[test]
    fn resolve_cache_dir_ignores_a_windows_root_without_a_drive() {
        for root in [r"\ProgramData\mwe-mcp", "cache"] {
            let dir = resolve_cache_dir(
                CacheHost::Windows,
                Some(std::ffi::OsStr::new(root)),
                None,
                Some(std::ffi::OsStr::new(r"D:\Users\u\AppData\Local")),
                "bge-m3",
            );
            assert_eq!(dir, under(r"D:\Users\u\AppData\Local"), "root {root}");
        }
    }

    /// Nothing in the environment to go on: the relative last resort. It is
    /// the case the Windows branch exists to keep a Windows daemon out of.
    #[test]
    fn resolve_cache_dir_last_resort_is_relative() {
        for host in [CacheHost::Unix, CacheHost::Windows] {
            assert_eq!(
                resolve_cache_dir(host, None, None, None, "bge-m3"),
                under(".cache"),
                "{host:?}"
            );
        }
    }

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
