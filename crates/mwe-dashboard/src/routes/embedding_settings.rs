// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editor for `<workdir>/mwe-mcp.config.yaml > embedding` —
//! the embedder backend selector + device toggle (roadmap 18e).
//!
//! Two routes, both behind [`AdminUser`]:
//!
//! - GET  `/admin/embedding` — render the current `embedding:` config.
//! - POST `/admin/embedding` — atomic save of the YAML (backup `.bak`,
//!   serialize, atomic-write), preserving every other section.
//!
//! Unlike [recall settings](super::recall_settings), the embedder is built
//! **once at startup** (it backs recall / capture / reindex), so a change
//! here takes effect on the **next server restart**, not the next turn —
//! there is no live handle to hot-swap. When the backend or model changes
//! the store's vectors no longer match the new embedder, so a full reindex
//! is needed; the embedder-identity guard (roadmap 18g) warns on the next
//! startup.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, EmbeddingConfig, bundled_embedder_available};
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/embedding`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/embedding", get(page).post(save))
}

/// Selectable backends: `(value, label)`.
const BACKENDS: &[(&str, &str)] = &[
    ("ollama", "Ollama (HTTP — local / remote)"),
    ("bundled", "Bundled (in-binary Candle / bge-m3)"),
    ("openai", "OpenAI-compatible (not yet wired)"),
];

struct Flash<'a> {
    kind: &'static str,
    msg: &'a str,
}

// ---------- GET ----------

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let workdir = workdir_of(&state)?;
    let cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    Ok(Html(render(admin.session(), &cfg.embedding, None)))
}

fn render(
    session: &crate::auth::SessionUser,
    cfg: &EmbeddingConfig,
    flash: Option<Flash<'_>>,
) -> String {
    let bundled_ok = bundled_embedder_available();
    // GPU acceleration needs a CUDA-enabled build (roadmap 18f); the shipped
    // default artifact is CPU-only, so the GPU option stays disabled here.
    let gpu_ok = false;

    let body: Markup = html! {
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        p.flash.flash-info {
            strong { "Changes take effect on the next server restart." }
            " The embedder is built once at startup; saving rewrites "
            code { (CONFIG_FILENAME) } " atomically. If you change the "
            strong { "backend" } " or " strong { "model" } ", the stored "
            "vectors no longer match the new embedder — run a full reindex "
            "(the embedder-identity guard warns on the next startup)."
        }

        h2 { "Embedding settings" }
        p.muted {
            "Which embedder backs recall, capture-time dedup and search — the "
            code { "embedding:" } " section of " code { (CONFIG_FILENAME) }
            ". Default: Ollama " code { "bge-m3" } " on localhost."
        }

        (embedding_form(cfg, bundled_ok, gpu_ok))

        p.muted {
            "Related: the generative LLM slots live in the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; recall resource knobs in "
            a href="/dashboard/admin/recall-settings" { "Recall settings" } "."
        }
    };
    layout::authenticated_page("Embedding settings", session, &body)
}

/// The `<form>` block — split out of [`render`] to keep that function within
/// the line budget. `bundled_ok` / `gpu_ok` gate the backend + device options
/// to what this build actually supports.
fn embedding_form(cfg: &EmbeddingConfig, bundled_ok: bool, gpu_ok: bool) -> Markup {
    html! {
        form action="/dashboard/admin/embedding" method="post" {
            table.config-table {
                tbody {
                    tr {
                        td { label for="backend" { "Backend" } }
                        td {
                            select id="backend" name="backend" {
                                @for &(val, lbl) in BACKENDS {
                                    @let disabled = val == "bundled" && !bundled_ok;
                                    option value=(val) selected[cfg.backend.as_str() == val]
                                        disabled[disabled] {
                                        (lbl)
                                        @if disabled { " — not in this build" }
                                    }
                                }
                            }
                        }
                        td.muted {
                            "bundled needs a build with the " code { "local-embedder" }
                            " feature"
                            @if !bundled_ok { " (this build: not compiled)" }
                            "."
                        }
                    }
                    tr {
                        td { label for="model" { "Model" } }
                        td {
                            input id="model" name="model" type="text"
                                value=(cfg.model) placeholder="bge-m3";
                        }
                        td.muted {
                            "Ollama: the wire model name. Bundled: the stable model id "
                            "(cache key + reindex check)."
                        }
                    }
                    tr {
                        td { label for="base_url" { "Ollama base URL" } }
                        td {
                            input id="base_url" name="base_url" type="text"
                                value=(cfg.base_url.clone().unwrap_or_default())
                                placeholder="http://localhost:11434";
                        }
                        td.muted { "Ollama endpoint override. Ignored by other backends." }
                    }
                    tr {
                        td { label for="device" { "Device (bundled)" } }
                        td {
                            select id="device" name="device" {
                                option value="cpu" selected[cfg.device.as_str() == "cpu"] { "CPU" }
                                option value="gpu" selected[cfg.device.as_str() == "gpu"]
                                    disabled[!gpu_ok] {
                                    "GPU"
                                    @if !gpu_ok { " — needs a CUDA build (roadmap 18f)" }
                                }
                            }
                        }
                        td.muted {
                            "CPU is the portable default. GPU requires a CUDA-enabled build."
                        }
                    }
                    tr {
                        td { label for="dimensions" { "Dimensions" } }
                        td {
                            input id="dimensions" name="dimensions" type="number" min="1"
                                value=(cfg.dimensions.to_string()) placeholder="1024";
                        }
                        td.muted { "Vector size the model emits (Ollama). bge-m3 = 1024." }
                    }
                    tr {
                        td { label for="model_dir" { "Bundled model dir (offline)" } }
                        td {
                            input id="model_dir" name="model_dir" type="text"
                                value=(cfg.model_dir.clone().unwrap_or_default())
                                placeholder="(auto-download to the XDG cache)";
                        }
                        td.muted {
                            "Bundled only: a local weights directory. Empty → auto-download "
                            "to the XDG cache on first use."
                        }
                    }
                }
            }
            p { button type="submit" { "Save embedding settings" } }
        }
    }
}

// ---------- POST ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_form(&form)?;

    // Preserve every non-embedding section by re-loading from disk (raw
    // — env overrides are runtime-only and must never be baked into the
    // file by a save) and replacing only the `embedding` field.
    // Config::load_raw returns Default when the file is missing — the
    // first save materialises it.
    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    let prev = cfg.embedding.clone();
    cfg.embedding = parsed.clone();

    // Backup `.bak` of the live YAML (if any) before overwriting.
    let path = workdir.join(CONFIG_FILENAME);
    let backup = backup_path_for(&path);
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&backup, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }

    let yaml = serde_yaml::to_string(&cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;

    tracing::info!(
        admin = %admin.session().sender_id,
        path = %path.display(),
        "embedding-settings: saved from dashboard (restart required to take effect)"
    );

    // A backend/model change re-points the vectors → reindex needed.
    let reindex = prev.backend != parsed.backend || prev.model != parsed.model;
    let msg = if reindex {
        "Embedding settings saved. The backend or model changed — restart the server AND run a \
         full reindex; the embedder-identity guard will flag the mismatch on the next startup."
    } else {
        "Embedding settings saved. Restart the server for the change to take effect."
    };

    let body = render(
        admin.session(),
        &parsed,
        Some(Flash {
            kind: "success",
            msg,
        }),
    );
    Ok(Html(body).into_response())
}

/// Decode the flat form into an [`EmbeddingConfig`], starting from the Rust
/// defaults so an empty field keeps the default. Unknown `backend` / `device`
/// values and a malformed `dimensions` are hard 422s naming the field.
fn parse_form(form: &HashMap<String, String>) -> Result<EmbeddingConfig> {
    let val = |k: &str| {
        form.get(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };

    let mut e = EmbeddingConfig::default();
    if let Some(backend) = val("backend") {
        if !["ollama", "bundled", "openai"].contains(&backend.as_str()) {
            return Err(DashboardError::Validation(format!(
                "unknown embedding backend `{backend}`"
            )));
        }
        e.backend = backend;
    }
    if let Some(model) = val("model") {
        e.model = model;
    }
    e.base_url = val("base_url");
    if let Some(device) = val("device") {
        if !["cpu", "gpu"].contains(&device.as_str()) {
            return Err(DashboardError::Validation(format!(
                "unknown embedding device `{device}` (expected `cpu` or `gpu`)"
            )));
        }
        e.device = device;
    }
    if let Some(dim) = val("dimensions") {
        e.dimensions = dim.parse::<usize>().map_err(|_| {
            DashboardError::Validation("`dimensions` must be a positive integer".to_owned())
        })?;
    }
    e.model_dir = val("model_dir");
    Ok(e)
}

/// Workdir root for the YAML path — only present on the full `mwe-mcp serve`
/// build, where the memory handles carry the workdir.
fn workdir_of(state: &DashboardState) -> Result<PathBuf> {
    state
        .memory
        .as_ref()
        .map(|m| m.workdir.clone())
        .ok_or_else(|| {
            DashboardError::Internal(
                "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
            )
        })
}

fn backup_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn empty_form_yields_defaults() {
        let e = parse_form(&form(&[])).expect("parse");
        assert_eq!(e, EmbeddingConfig::default());
        assert_eq!(e.backend, "ollama");
        assert_eq!(e.model, "bge-m3");
        assert_eq!(e.device, "cpu");
        assert_eq!(e.dimensions, 1024);
        assert!(e.base_url.is_none());
        assert!(e.model_dir.is_none());
    }

    #[test]
    fn full_form_roundtrips() {
        let e = parse_form(&form(&[
            ("backend", "bundled"),
            ("model", "bge-m3"),
            ("base_url", " "), // whitespace → None
            ("device", "cpu"),
            ("dimensions", "1024"),
            ("model_dir", "/opt/models/bge-m3"),
        ]))
        .expect("parse");
        assert_eq!(e.backend, "bundled");
        assert!(e.base_url.is_none(), "blank base_url must be None");
        assert_eq!(e.model_dir.as_deref(), Some("/opt/models/bge-m3"));
    }

    #[test]
    fn unknown_backend_is_rejected() {
        assert!(matches!(
            parse_form(&form(&[("backend", "weaviate")])),
            Err(DashboardError::Validation(_))
        ));
    }

    #[test]
    fn unknown_device_is_rejected() {
        assert!(matches!(
            parse_form(&form(&[("device", "tpu")])),
            Err(DashboardError::Validation(_))
        ));
    }

    #[test]
    fn malformed_dimensions_is_rejected() {
        assert!(matches!(
            parse_form(&form(&[("dimensions", "lots")])),
            Err(DashboardError::Validation(_))
        ));
    }
}
