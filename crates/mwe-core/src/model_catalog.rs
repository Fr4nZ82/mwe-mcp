// SPDX-License-Identifier: AGPL-3.0-or-later
//! models.dev catalog — provider/model metadata for the admin LLM picker.
//!
//! The source of truth is <https://models.dev/api.json>, a
//! community-maintained database of model metadata (context window, per-
//! million-token cost, capability flags). We ship a compact, filtered
//! snapshot — the three buildable cloud backends `anthropic` / `google`
//! (Gemini) / `openrouter` — vendored at `assets/model-catalog.json` and
//! embedded at compile time, so the picker works fully offline. [`refresh`]
//! re-fetches the live data into a workdir cache that [`load`] prefers over
//! the bundled copy, so a model id added upstream shows up without a
//! rebuild.
//!
//! Ollama is intentionally absent: its installed models are discovered live
//! from `/api/tags` (the registry is not listable), so the dashboard
//! sources that backend's list from the running server, not from here.
//!
//! The dashboard uses this to populate the model combobox (a free-text
//! `datalist`) and the per-model metadata strip; see the admin LLM config
//! wiki page.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Compact snapshot embedded at compile time (offline-first fallback).
const BUNDLED: &str = include_str!("../assets/model-catalog.json");

/// models.dev API endpoint [`refresh`] fetches.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Filename of the workdir catalog cache written by [`refresh`] and
/// preferred by [`load`].
pub const CACHE_FILENAME: &str = "model-catalog.json";

/// models.dev provider ids we keep, in picker display order. Note Google's
/// models.dev id is `google` while the mwe-mcp backend tag is `gemini`
/// (see [`catalog_key_for_backend`]).
const CATALOG_PROVIDERS: &[&str] = &["anthropic", "google", "openrouter"];

/// Map an mwe-mcp `backend` tag to the models.dev provider key. Returns
/// `None` for backends without a catalog here — `ollama` (discovered live
/// from `/api/tags`) and `openai` (gated / not buildable).
#[must_use]
pub fn catalog_key_for_backend(backend: &str) -> Option<&'static str> {
    match backend {
        "anthropic" => Some("anthropic"),
        "gemini" => Some("google"),
        "openrouter" => Some("openrouter"),
        _ => None,
    }
}

/// One model's metadata, trimmed to what the picker renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Model id passed to the backend (for `OpenRouter`, the `vendor/model`
    /// slug with any models.dev `~` alias marker stripped).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Context-window size in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Max output tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    /// USD per million input tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost: Option<f64>,
    /// USD per million output tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost: Option<f64>,
    /// Accepts image input (vision).
    #[serde(default)]
    pub vision: bool,
    /// Supports function/tool calling.
    #[serde(default)]
    pub tools: bool,
    /// Has an extended-thinking / reasoning mode.
    #[serde(default)]
    pub reasoning: bool,
}

/// The catalog: models grouped by models.dev provider id. Serialises
/// transparently to/from the `{ "anthropic": [...], ... }` snapshot shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog(BTreeMap<String, Vec<CatalogModel>>);

impl Catalog {
    /// Models known for an mwe-mcp `backend` tag (mapping `gemini` →
    /// models.dev `google`). Empty slice for `ollama` / unknown backends.
    #[must_use]
    pub fn models_for(&self, backend: &str) -> &[CatalogModel] {
        catalog_key_for_backend(backend)
            .and_then(|k| self.0.get(k))
            .map_or(&[], Vec::as_slice)
    }

    /// Look up a single model's metadata by backend + id.
    #[must_use]
    pub fn lookup(&self, backend: &str, model_id: &str) -> Option<&CatalogModel> {
        self.models_for(backend).iter().find(|m| m.id == model_id)
    }

    /// Total model count across all providers (for diagnostics / the
    /// refresh flash message).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.values().map(Vec::len).sum()
    }

    /// True when the catalog holds no models (an unparseable cache).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }
}

/// The embedded snapshot, parsed once.
fn bundled() -> &'static Catalog {
    static CELL: OnceLock<Catalog> = OnceLock::new();
    CELL.get_or_init(|| serde_json::from_str(BUNDLED).unwrap_or_default())
}

/// Load the catalog, preferring a fresh workdir cache over the bundled
/// snapshot.
///
/// The cache is written by [`refresh`]; a missing or unparseable cache
/// falls back silently to the embedded copy, so the picker never breaks.
#[must_use]
pub fn load(workdir: &Path) -> Catalog {
    let cache = workdir.join(CACHE_FILENAME);
    if let Ok(body) = std::fs::read_to_string(&cache)
        && let Ok(cat) = serde_json::from_str::<Catalog>(&body)
        && !cat.is_empty()
    {
        return cat;
    }
    bundled().clone()
}

/// Fetch the live models.dev catalog, compact it to our schema, and
/// atomic-write it to `<workdir>/model-catalog.json` so [`load`] picks it
/// up. Returns the number of models written.
///
/// # Errors
///
/// [`CatalogError::Fetch`] on a transport / HTTP error, [`CatalogError::Serialize`]
/// when the compacted catalog cannot be serialised, [`CatalogError::Write`]
/// when the cache file cannot be written.
pub async fn refresh(workdir: &Path) -> Result<usize, CatalogError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let raw: Value = client
        .get(MODELS_DEV_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let catalog = compact_from_models_dev(&raw);
    let total = catalog.len();
    let json = serde_json::to_string(&catalog)?;
    crate::wiki::atomic_write(&workdir.join(CACHE_FILENAME), json.as_bytes())
        .map_err(|e| CatalogError::Write(e.to_string()))?;
    Ok(total)
}

/// Compact a raw models.dev `api.json` value into our schema, keeping only
/// [`CATALOG_PROVIDERS`] and the fields the picker needs. Mirrors the jq
/// transform that produced the bundled snapshot so a refresh and the
/// vendored copy stay shape-compatible.
fn compact_from_models_dev(raw: &Value) -> Catalog {
    let mut out: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::new();
    for provider in CATALOG_PROVIDERS {
        let Some(models) = raw
            .get(provider)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let mut list: Vec<CatalogModel> = models.values().filter_map(slim_model).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        out.insert((*provider).to_owned(), list);
    }
    Catalog(out)
}

/// Project one models.dev model object onto [`CatalogModel`]. Returns
/// `None` only when the entry has no usable `id`.
fn slim_model(v: &Value) -> Option<CatalogModel> {
    // OpenRouter ids carry a leading `~` alias marker in models.dev; the
    // real slug the backend wants drops it.
    let id = v.get("id")?.as_str()?.trim_start_matches('~').to_owned();
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id.as_str())
        .to_owned();
    Some(CatalogModel {
        id,
        name,
        context: v
            .pointer("/limit/context")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        max_output: v
            .pointer("/limit/output")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        input_cost: v.pointer("/cost/input").and_then(Value::as_f64),
        output_cost: v.pointer("/cost/output").and_then(Value::as_f64),
        vision: v
            .get("attachment")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tools: v.get("tool_call").and_then(Value::as_bool).unwrap_or(false),
        reasoning: v.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Errors raised by [`refresh`].
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Transport / HTTP failure fetching models.dev.
    #[error("models.dev fetch failed: {0}")]
    Fetch(#[from] reqwest::Error),
    /// The compacted catalog could not be serialised.
    #[error("catalog serialise failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The cache file could not be written.
    #[error("catalog write failed: {0}")]
    Write(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_parses_and_has_models() {
        let cat = bundled();
        assert!(!cat.is_empty(), "bundled catalog must not be empty");
        assert!(
            !cat.models_for("anthropic").is_empty(),
            "anthropic models present"
        );
        assert!(
            !cat.models_for("openrouter").is_empty(),
            "openrouter models present"
        );
    }

    #[test]
    fn gemini_backend_maps_to_google_provider() {
        assert_eq!(catalog_key_for_backend("gemini"), Some("google"));
        assert_eq!(catalog_key_for_backend("anthropic"), Some("anthropic"));
        assert_eq!(catalog_key_for_backend("openrouter"), Some("openrouter"));
        // No catalog for the live-discovered / gated backends.
        assert_eq!(catalog_key_for_backend("ollama"), None);
        assert_eq!(catalog_key_for_backend("openai"), None);
        // The `gemini` tag actually resolves to populated `google` models.
        let cat = bundled();
        assert!(!cat.models_for("gemini").is_empty());
    }

    #[test]
    fn bundled_anthropic_entry_has_metadata() {
        let cat = bundled();
        let sonnet = cat
            .models_for("anthropic")
            .iter()
            .find(|m| m.id.contains("sonnet"))
            .expect("a sonnet model exists");
        assert!(sonnet.context.is_some(), "context window known");
        assert!(sonnet.input_cost.is_some(), "input cost known");
        assert!(sonnet.vision, "claude sonnet accepts images");
        assert!(sonnet.tools, "claude sonnet supports tools");
    }

    #[test]
    fn compact_from_models_dev_strips_openrouter_alias_marker() {
        let raw = serde_json::json!({
            "openrouter": { "models": {
                "~anthropic/claude-sonnet-latest": {
                    "id": "~anthropic/claude-sonnet-latest",
                    "name": "Anthropic Claude Sonnet Latest",
                    "attachment": true,
                    "tool_call": true,
                    "reasoning": true,
                    "limit": { "context": 1_000_000, "output": 128_000 },
                    "cost": { "input": 3, "output": 15 }
                }
            }},
            // A provider we don't keep — must be ignored.
            "mistral": { "models": { "x": { "id": "x", "name": "X" } } }
        });
        let cat = compact_from_models_dev(&raw);
        let models = cat.models_for("openrouter");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "anthropic/claude-sonnet-latest");
        assert_eq!(models[0].context, Some(1_000_000));
        assert!((models[0].input_cost.expect("cost") - 3.0).abs() < f64::EPSILON);
        assert!(models[0].vision && models[0].tools && models[0].reasoning);
        // Unkept provider absent.
        assert!(cat.models_for("gemini").is_empty());
    }

    #[test]
    fn load_falls_back_to_bundled_when_no_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cat = load(dir.path());
        assert!(!cat.is_empty(), "falls back to the embedded snapshot");
    }

    #[test]
    fn load_prefers_a_valid_workdir_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join(CACHE_FILENAME);
        std::fs::write(
            &cache,
            serde_json::json!({
                "anthropic": [
                    { "id": "only-model", "name": "Only", "vision": false, "tools": false, "reasoning": false }
                ]
            })
            .to_string(),
        )
        .expect("seed cache");
        let cat = load(dir.path());
        assert_eq!(cat.models_for("anthropic").len(), 1);
        assert_eq!(cat.models_for("anthropic")[0].id, "only-model");
    }

    #[test]
    fn empty_cache_falls_back_to_bundled() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(CACHE_FILENAME),
            serde_json::json!({ "anthropic": [], "google": [], "openrouter": [] }).to_string(),
        )
        .expect("seed empty");
        let cat = load(dir.path());
        // An all-empty cache is treated as unusable → bundled fallback.
        assert!(!cat.is_empty());
    }
}
