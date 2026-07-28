// SPDX-License-Identifier: AGPL-3.0-or-later
//! Training spool — records internal-LLM prompt/completion pairs as
//! JSONL for later distillation fine-tuning of the local slot models.
//!
//! ## Why
//!
//! The internal LLM slots (`ingest`, `navigator`, …) are narrow,
//! structured tasks — exactly the shape where a small local model
//! fine-tuned on traces from a strong API "teacher" model approaches
//! the teacher's quality at zero marginal cost. The blocker is the
//! dataset. This module makes production generate it as a side effect:
//! when enabled, every [`crate::llm::LlmBackend`] call made through
//! [`crate::config::LlmFunctionConfig::build_backend`] appends one
//! JSON line — slot, backend, model, full request, full response — to
//! a per-day file under `<workdir>/training-spool/`.
//!
//! ## Design
//!
//! - **Decorator, not call sites** — [`SpoolingBackend`] wraps the
//!   built backend behind the same [`crate::llm::LlmBackend`] trait;
//!   `build_backend` wraps unconditionally via [`maybe_wrap`], so every
//!   slot and every transport (MCP ingest, REM cycle, dashboard chat)
//!   is covered without touching a single call site.
//! - **Process-wide handle** — one [`TrainingSpool`] per process,
//!   installed at startup by the server binary (it captures the
//!   workdir), same idiom as [`crate::oauth::install_global_store`].
//!   Library users that never install one (tests, embedded use) get
//!   passthrough backends.
//! - **Hot-toggleable** — the enabled flag is an `AtomicBool` checked
//!   per call. The dashboard panel persists the operator's choice in
//!   `mwe-mcp.config.yaml` (`training_spool.enabled`) and flips the
//!   installed handle in place; no restart.
//! - **Best-effort** — recording failures log a warning and never fail
//!   the LLM call (same doctrine as `recall_trace`).
//! - **Full fidelity, minus images** — prompts and completions are
//!   recorded verbatim (a truncated prompt is useless as a training
//!   pair); image attachments are recorded as count + MIME types only,
//!   the base64 payload is dropped.
//!
//! Health checks are delegated to the inner backend and never recorded
//! — a `ping` probe is noise in a training set.
//!
//! ## Record shape
//!
//! One JSON object per line, `kind: "complete" | "chat"`:
//!
//! ```json
//! {"ts":"2026-07-19T09:00:00Z","function":"ingest","backend":"anthropic",
//!  "model":"claude-opus-4-7","kind":"complete",
//!  "request":{"system":"…","prompt":"…","max_tokens":1024,"temperature":0.1,
//!             "stop":[],"images":[]},
//!  "response":{"text":"…","finish_reason":"end_of_turn",
//!              "usage":{"prompt_tokens":123,"completion_tokens":45,
//!                        "cached_prompt_tokens":110}}}
//! ```
//!
//! The spool holds raw prompts — which embed recalled memory content —
//! so the directory stays on the server, owner-readable, and the
//! toggle defaults to **off**. See `docs/` (internal LLM functions)
//! for the operational guidance.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::json;

use crate::config::LlmFunction;
use crate::llm::{
    ChatRequest, ChatResponse, CompletionRequest, CompletionResponse, LlmBackend, Result,
};

/// Workdir subdirectory holding the per-day JSONL spool files.
pub const TRAINING_SPOOL_DIR: &str = "training-spool";

/// Process-wide spool handle, installed once by the server binary at
/// startup (it captures the workdir). `build_backend` reads it via
/// [`maybe_wrap`]. A `OnceLock` because there is one spool per server
/// process (one workdir) — same idiom as the OAuth login store.
static GLOBAL_SPOOL: OnceLock<Arc<TrainingSpool>> = OnceLock::new();

/// Install the process-wide spool (first call wins; idempotent).
pub fn install_global(spool: Arc<TrainingSpool>) {
    let _ = GLOBAL_SPOOL.set(spool);
}

/// The process-wide spool, if [`install_global`] has run.
#[must_use]
pub fn global() -> Option<Arc<TrainingSpool>> {
    GLOBAL_SPOOL.get().cloned()
}

/// Wrap `inner` in a [`SpoolingBackend`] when a process-wide spool is
/// installed; passthrough otherwise.
///
/// Called by [`crate::config::LlmFunctionConfig::build_backend`] on
/// every built backend — the enabled check happens per call, not here,
/// so a runtime toggle flip is observed by backends built before it.
#[must_use]
pub fn maybe_wrap(
    inner: Box<dyn LlmBackend>,
    function: LlmFunction,
    backend_tag: &str,
) -> Box<dyn LlmBackend> {
    match global() {
        Some(spool) => Box::new(SpoolingBackend {
            inner,
            function,
            backend_tag: backend_tag.to_owned(),
            spool,
        }),
        None => inner,
    }
}

/// The spool itself: target directory + the hot-toggleable enabled
/// flag. One per process, shared via `Arc` between the recording
/// decorators and the dashboard panel that flips it.
pub struct TrainingSpool {
    /// `<workdir>/training-spool` — created lazily on first record.
    dir: PathBuf,
    /// Hot flag — flipped in place by the dashboard panel.
    enabled: AtomicBool,
    /// Serialises concurrent appends so JSONL lines never interleave.
    write_lock: Mutex<()>,
}

impl TrainingSpool {
    /// Build a spool rooted at `<workdir>/training-spool`, initially
    /// enabled per the loaded `training_spool.enabled` config value.
    #[must_use]
    pub fn new(workdir: &Path, enabled: bool) -> Self {
        Self {
            dir: workdir.join(TRAINING_SPOOL_DIR),
            enabled: AtomicBool::new(enabled),
            write_lock: Mutex::new(()),
        }
    }

    /// Directory the spool appends into.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Current state of the hot flag.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flip the hot flag (the dashboard panel calls this after
    /// persisting the choice to YAML — disk first, then the flip).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Record one `complete` exchange. No-op when disabled.
    pub fn record_complete(
        &self,
        function: LlmFunction,
        backend_tag: &str,
        model: &str,
        request: &CompletionRequest,
        response: &CompletionResponse,
    ) {
        if !self.is_enabled() {
            return;
        }
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "function": function.yaml_key(),
            "backend": backend_tag,
            "model": model,
            "kind": "complete",
            "request": {
                "system": request.system,
                "prompt": request.prompt,
                "max_tokens": request.max_tokens,
                "temperature": request.temperature,
                "stop": request.stop,
                // Payload dropped deliberately: base64 image bytes would
                // bloat the spool and vision pairs need a different
                // training pipeline anyway.
                "images": request.images.iter().map(|i| &i.mime_type).collect::<Vec<_>>(),
            },
            "response": {
                "text": response.text,
                "finish_reason": finish_reason_str(response.finish_reason),
                "usage": {
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                    // Prefix-cache read (roadmap 49): the observable that
                    // says whether a prompt's repeated standing half is
                    // actually billed once. Null on backends that don't
                    // report it; 0 on a genuine miss.
                    "cached_prompt_tokens": response.usage.cached_prompt_tokens,
                },
            },
        });
        self.append(&record);
    }

    /// Record one `chat` exchange (the dashboard agentic loop). One
    /// record per model turn — the full message history rides along, so
    /// the last line of a loop supersedes the earlier ones for
    /// training purposes. No-op when disabled.
    pub fn record_chat(
        &self,
        function: LlmFunction,
        backend_tag: &str,
        model: &str,
        request: &ChatRequest,
        response: &ChatResponse,
    ) {
        if !self.is_enabled() {
            return;
        }
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "function": function.yaml_key(),
            "backend": backend_tag,
            "model": model,
            "kind": "chat",
            "request": {
                "messages": request.messages.iter().map(chat_message_json).collect::<Vec<_>>(),
                "tools": request.tools.iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })).collect::<Vec<_>>(),
                "max_tokens": request.max_tokens,
                "temperature": request.temperature,
            },
            "response": {
                "message": chat_message_json(&response.message),
                "finish_reason": finish_reason_str(response.finish_reason),
                "usage": {
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                    // Prefix-cache read (roadmap 49): the observable that
                    // says whether a prompt's repeated standing half is
                    // actually billed once. Null on backends that don't
                    // report it; 0 on a genuine miss.
                    "cached_prompt_tokens": response.usage.cached_prompt_tokens,
                },
            },
        });
        self.append(&record);
    }

    /// Append one record to today's file. Best-effort: any failure
    /// logs a warning and returns — recording must never fail a turn.
    fn append(&self, record: &serde_json::Value) {
        let file = self
            .dir
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let mut line = record.to_string();
        line.push('\n');
        let Ok(_guard) = self.write_lock.lock() else {
            tracing::warn!("training-spool: write lock poisoned — record dropped");
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(dir = %self.dir.display(), error = %e, "training-spool: cannot create dir — record dropped");
            return;
        }
        let opened = OpenOptions::new().create(true).append(true).open(&file);
        match opened {
            Ok(mut f) => {
                if let Err(e) = f.write_all(line.as_bytes()) {
                    tracing::warn!(file = %file.display(), error = %e, "training-spool: append failed — record dropped");
                }
            },
            Err(e) => {
                tracing::warn!(file = %file.display(), error = %e, "training-spool: open failed — record dropped");
            },
        }
    }
}

/// Coarse wire string for [`crate::llm::FinishReason`].
const fn finish_reason_str(reason: crate::llm::FinishReason) -> &'static str {
    match reason {
        crate::llm::FinishReason::EndOfTurn => "end_of_turn",
        crate::llm::FinishReason::MaxTokens => "max_tokens",
        crate::llm::FinishReason::Other => "other",
    }
}

/// JSON shape of one chat turn, tool calls included.
fn chat_message_json(m: &crate::llm::ChatMessage) -> serde_json::Value {
    json!({
        "role": m.role.as_str(),
        "content": m.content,
        "tool_calls": m.tool_calls.iter().map(|c| json!({
            "id": c.id,
            "name": c.name,
            "arguments": c.arguments,
        })).collect::<Vec<_>>(),
        "tool_call_id": m.tool_call_id,
    })
}

/// Decorator that records successful exchanges through the wrapped
/// backend. Errors pass through unrecorded — a failed call has no
/// completion to learn from.
struct SpoolingBackend {
    inner: Box<dyn LlmBackend>,
    function: LlmFunction,
    backend_tag: String,
    spool: Arc<TrainingSpool>,
}

#[async_trait]
impl LlmBackend for SpoolingBackend {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let response = self.inner.complete(request.clone()).await?;
        self.spool.record_complete(
            self.function,
            &self.backend_tag,
            self.inner.model_id(),
            &request,
            &response,
        );
        Ok(response)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let response = self.inner.chat(request.clone()).await?;
        self.spool.record_chat(
            self.function,
            &self.backend_tag,
            self.inner.model_id(),
            &request,
            &response,
        );
        Ok(response)
    }

    async fn health_check(&self) -> Result<()> {
        // Delegate so a backend's cheap liveness probe stays cheap —
        // and so probe pings never pollute the training set.
        self.inner.health_check().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FakeLlmBackend;

    fn spool_in(dir: &Path, enabled: bool) -> Arc<TrainingSpool> {
        Arc::new(TrainingSpool::new(dir, enabled))
    }

    fn read_lines(spool: &TrainingSpool) -> Vec<serde_json::Value> {
        let Ok(entries) = std::fs::read_dir(spool.dir()) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            for line in content.lines() {
                lines.push(serde_json::from_str(line).unwrap());
            }
        }
        lines
    }

    #[tokio::test]
    async fn complete_is_recorded_with_full_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_in(tmp.path(), true);
        let backend = SpoolingBackend {
            inner: Box::new(FakeLlmBackend::new("fake-model", "the completion")),
            function: LlmFunction::Ingest,
            backend_tag: "anthropic".to_owned(),
            spool: Arc::clone(&spool),
        };
        let request = CompletionRequest::new("classify this")
            .with_system("you are the ingest classifier")
            .with_temperature(0.1);
        let response = backend.complete(request).await.unwrap();
        assert_eq!(response.text, "the completion");

        let lines = read_lines(&spool);
        assert_eq!(lines.len(), 1);
        let rec = &lines[0];
        assert_eq!(rec["function"], "ingest");
        assert_eq!(rec["backend"], "anthropic");
        assert_eq!(rec["kind"], "complete");
        assert_eq!(rec["request"]["prompt"], "classify this");
        assert_eq!(rec["request"]["system"], "you are the ingest classifier");
        assert_eq!(rec["response"]["text"], "the completion");
        assert_eq!(rec["response"]["finish_reason"], "end_of_turn");
    }

    #[tokio::test]
    async fn chat_is_recorded_with_messages_and_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_in(tmp.path(), true);
        let backend = SpoolingBackend {
            inner: Box::new(FakeLlmBackend::new("fake-model", "final answer")),
            function: LlmFunction::OperatorChat,
            backend_tag: "ollama".to_owned(),
            spool: Arc::clone(&spool),
        };
        let request = ChatRequest::new(vec![
            crate::llm::ChatMessage::system("sys"),
            crate::llm::ChatMessage::user("hello"),
        ])
        .with_tools(vec![crate::llm::Tool {
            name: "t".to_owned(),
            description: "d".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }]);
        backend.chat(request).await.unwrap();

        let lines = read_lines(&spool);
        assert_eq!(lines.len(), 1);
        let rec = &lines[0];
        assert_eq!(rec["kind"], "chat");
        assert_eq!(rec["function"], "operator_chat");
        assert_eq!(rec["request"]["messages"][1]["content"], "hello");
        assert_eq!(rec["request"]["tools"][0]["name"], "t");
        assert_eq!(rec["response"]["message"]["content"], "final answer");
    }

    #[tokio::test]
    async fn disabled_spool_records_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_in(tmp.path(), false);
        let backend = SpoolingBackend {
            inner: Box::new(FakeLlmBackend::new("fake-model", "reply")),
            function: LlmFunction::Ingest,
            backend_tag: "ollama".to_owned(),
            spool: Arc::clone(&spool),
        };
        backend
            .complete(CompletionRequest::new("hi"))
            .await
            .unwrap();
        assert!(read_lines(&spool).is_empty());
        assert!(!spool.dir().exists(), "disabled spool must not touch disk");
    }

    #[tokio::test]
    async fn toggle_flips_recording_at_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_in(tmp.path(), false);
        let backend = SpoolingBackend {
            inner: Box::new(FakeLlmBackend::new("fake-model", "reply")),
            function: LlmFunction::Navigator,
            backend_tag: "ollama".to_owned(),
            spool: Arc::clone(&spool),
        };
        backend
            .complete(CompletionRequest::new("one"))
            .await
            .unwrap();
        spool.set_enabled(true);
        backend
            .complete(CompletionRequest::new("two"))
            .await
            .unwrap();
        spool.set_enabled(false);
        backend
            .complete(CompletionRequest::new("three"))
            .await
            .unwrap();

        let lines = read_lines(&spool);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["request"]["prompt"], "two");
    }

    #[test]
    fn unwritable_dir_never_panics() {
        let tmp = tempfile::tempdir().unwrap();
        // A *file* where the spool dir should be makes create_dir_all fail.
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        let spool = TrainingSpool::new(&blocked.join("sub"), true);
        spool.record_complete(
            LlmFunction::Ingest,
            "ollama",
            "m",
            &CompletionRequest::new("p"),
            &CompletionResponse {
                text: "t".to_owned(),
                finish_reason: crate::llm::FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage::default(),
            },
        );
    }
}
