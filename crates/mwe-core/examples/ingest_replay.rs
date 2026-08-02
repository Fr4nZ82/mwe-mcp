// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ingest replay differential — the measurement instrument roadmap 49
//! (prompt mass) and 45b (golden set) both need.
//!
//! Every remaining lever on the ingest prompt — gate a section, retire a
//! section, split the classifier in two — has the same failure mode: the
//! decisions get quietly worse and nothing in the pipeline notices. This
//! harness replaces the guess with a measurement. It takes **real
//! production requests** out of the training spool, re-runs them against a
//! *modified* system prompt, and diffs the two answers field by field.
//!
//! The reference is not the recorded completion (it came from an older
//! prompt version): it is a fresh **baseline** run of today's bundled
//! prompt over the same requests. So a full experiment is three runs — the
//! baseline, a second baseline that measures the sampling noise floor, and
//! the variant — and the variant only counts as a regression when it drifts
//! further than the noise floor.
//!
//! ```text
//! # 1. baseline (today's bundled prompt, verbatim)
//! cargo run -p mwe-core --example ingest_replay --release -- \
//!     --spool ./work/training-spool --out base-a.jsonl
//! # 2. the noise floor: identical config, second sample
//! cargo run -p mwe-core --example ingest_replay --release -- \
//!     --spool ./work/training-spool --out base-b.jsonl
//! # 3. the variant under test
//! cargo run -p mwe-core --example ingest_replay --release -- \
//!     --spool ./work/training-spool --variant gated --out gated.jsonl
//! # 4. read the two diffs — variant-vs-base is only meaningful against base-vs-base
//! cargo run -p mwe-core --example ingest_replay -- --compare base-a.jsonl base-b.jsonl
//! cargo run -p mwe-core --example ingest_replay -- --compare base-a.jsonl gated.jsonl
//! ```
//!
//! Flags (all optional except where noted):
//!
//! | flag | default | meaning |
//! |---|---|---|
//! | `--spool <dir>` | `./work/training-spool` | directory of `*.jsonl` spool files |
//! | `--out <file>` | required for a replay | where results are written (JSONL) |
//! | `--variant <spec>` | `baseline` | `baseline`, `gated`, or `drop:<heading>;<heading>` |
//! | `--limit <n>` | all | replay only the newest `n` records |
//! | `--backend <b>` | `gemini` | `gemini` or `anthropic` — **replay the backend production runs** |
//! | `--model <id>` | `gemini-3-flash-preview` | model to replay against |
//! | `--prompt-file <f>` | the bundled `ingest.md` | replay an arbitrary prompt file — how a *previous* version becomes the baseline |
//! | `--reasoning-effort <e>` | slot default (`minimal`) | `low`/`medium`/`high` — how long the model deliberates |
//! | `--concurrency <n>` | `4` | requests in flight |
//! | `--dry-run` | off | build the prompts, print the cost projection, call nothing |
//! | `--compare <a> <b>` | — | diff two result files instead of replaying |
//!
//! The key is read from `GEMINI_API_KEY`, or `ANTHROPIC_API_KEY` under
//! `--backend anthropic`. **This tool spends** — money on Gemini or a Console
//! key, plan quota on a subscription token — so it is a manual example, never
//! part of `cargo test`. `--dry-run` costs nothing and reports exactly what a
//! real run would send.
//!
//! ## Replay the backend production actually runs
//!
//! A measurement is only transferable if it drives the slot the deployment
//! drives. Read `llm.<slot>.backend` / `.model` out of the deployment's
//! `mwe-mcp.config.yaml` and pass them. As of 2026-07-29 this deployment's
//! `ingest` slot is `anthropic` / `claude-haiku-4-5-20251001`, so:
//!
//! ```text
//! # baseline = the PREVIOUS prompt version, pulled out of git
//! git show <rev>:crates/mwe-core/prompts/ingest.md > /tmp/ingest-prev.md
//! ANTHROPIC_API_KEY=<setup-token> cargo run -p mwe-core --example ingest_replay --release -- \
//!     --spool ./work/training-spool --backend anthropic --model claude-haiku-4-5-20251001 \
//!     --prompt-file /tmp/ingest-prev.md --out base-a.jsonl
//! # variant = today's bundled prompt (no --prompt-file)
//! ANTHROPIC_API_KEY=<setup-token> cargo run -p mwe-core --example ingest_replay --release -- \
//!     --spool ./work/training-spool --backend anthropic --model claude-haiku-4-5-20251001 \
//!     --out variant.jsonl
//! ```
//!
//! **Use a `claude setup-token`, never a running server's login store.** The
//! store at `<workdir>/anthropic_oauth.json` refreshes and rewrites itself on
//! expiry; a second process refreshing the same rotating refresh token can
//! invalidate the credential the live server depends on. A setup token has no
//! refresh cycle and cannot do that.
//!
//! ## Fidelity caveats, stated up front
//!
//! - The spool drops image bytes (count + MIME only), so a turn that rode
//!   inline images replays **text-only**. Those records are counted and
//!   reported separately; do not read a diff on them as a prompt effect.
//! - The user message is replayed verbatim from the spool, but the system
//!   prompt is rebuilt from today's bundled `ingest.md` with the record's
//!   own locale directive. That is deliberate: it makes records from
//!   different prompt versions comparable.
//! - Sampling is not deterministic at the production temperature (0.1), so
//!   a zero-disagreement result is not expected even for two identical
//!   runs. That is what the noise-floor run is for.

// A measurement harness: the ratios below divide small integer counts for
// a human-readable report, where the precision-loss lint is pure noise.
#![allow(clippy::cast_precision_loss)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use mwe_core::llm::{
    AnthropicApiKey, AnthropicBackend, CompletionRequest, GeminiApiKey, GeminiBackend, LlmBackend,
    LlmError,
};
use mwe_core::prompts;
use serde_json::{Value, json};

/// The bundled ingest prompt — the same string `ingest.rs` embeds, so a
/// replay always measures the prompt currently in the tree.
const BUNDLED_INGEST_PROMPT_MD: &str = include_str!("../prompts/ingest.md");

/// Opening of the ingest classifier's system prompt. The spool labels a
/// record by **slot**, and the `ingest` slot is shared with `cronista`,
/// `conciliatore`, `regenerate-index` and `ingest-closures` — filtering on
/// `function` alone over-counts by ~75%. This marker is the real filter.
const INGEST_MARKER: &str = "You are the `ingest` classifier inside mwe-mcp";

/// Heading that opens the locale directive, which `ingest.rs` substitutes
/// into `{locale}` as the very last thing in the prompt.
const LANGUAGE_HEADING: &str = "## LANGUAGE\n\n";

/// Production's ingest sampling temperature (`ingest.rs`).
const REPLAY_TEMPERATURE: f32 = 0.1;

/// Production's ingest completion cap (`ingest.rs`).
const REPLAY_MAX_TOKENS: u32 = 4096;

/// Env var carrying the Gemini key, same name the server config uses.
const API_KEY_ENV: &str = "GEMINI_API_KEY";

/// Credential for `--backend anthropic`. The backend classifies it **by
/// value**: `sk-ant-oat-…` / `cc-…` / `eyJ…` are Claude Code subscription
/// tokens (Bearer auth, no per-token invoice), `sk-ant-api…` is a Console
/// key and bills per token.
///
/// A **setup token** (`claude setup-token`) is the credential to use here:
/// it is long-lived and carries no refresh cycle, so this harness can never
/// rotate — and thereby invalidate — the token in a *server's* login store
/// (`<workdir>/anthropic_oauth.json`). That is why the harness deliberately
/// does NOT offer [`AnthropicBackend::with_login_store`]: pointing a second
/// process at a live deployment's store means two refreshers racing over one
/// rotating refresh token.
const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// `true` when the credential is a subscription token rather than a Console
/// API key. Mirrors `llm::is_anthropic_oauth_token`, which is private — used
/// only to tell the operator which meter their run is about to spin.
fn looks_like_subscription_token(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token.starts_with("sk-ant-api") {
        return false;
    }
    token.starts_with("sk-ant-") || token.starts_with("eyJ") || token.starts_with("cc-")
}

/// Attempts per record before it is written off as failed. A replay of a
/// few hundred records at any useful concurrency *will* meet the
/// provider's rate limiter; without a retry the run silently loses half
/// its sample, which is worse than useless — a differential computed over
/// a biased survivor set reads like a result.
const MAX_ATTEMPTS: u32 = 5;

/// First back-off step, doubled per attempt (1s, 2s, 4s, 8s).
const BACKOFF_BASE_MS: u64 = 1_000;

// ---------- Records ----------

/// One replayable production request, reconstructed from a spool line.
struct Record {
    /// Spool timestamp — the join key between two result files.
    ts: String,
    /// The locale directive this turn was rendered with, lifted out of the
    /// recorded system prompt's `## LANGUAGE` tail.
    locale: String,
    /// The recorded user message (`build_prompt` output), replayed verbatim.
    user: String,
    /// This turn fed the agent's own reply back (roadmap 27) — the gate for
    /// Part 12.
    author_is_assistant: bool,
    /// This turn carried media — the gate for Part 9.
    has_attachments: bool,
    /// The turn rode inline images whose bytes the spool dropped, so the
    /// replay is not faithful for it.
    lossy_images: bool,
    /// Who spoke this turn, as a principal (`user:<id>`), parsed off the
    /// first line of the recorded user message. Needed to compare
    /// *audiences* rather than raw ACL fields — see [`audience`].
    sender: String,
}

/// Read every `*.jsonl` under `dir` and reconstruct the ingest-classifier
/// requests, newest last.
fn load_records(dir: &Path) -> Result<Vec<Record>, String> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read spool dir {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in &files {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("function").and_then(Value::as_str) != Some("ingest") {
                continue;
            }
            let request = &row["request"];
            let system = request["system"].as_str().unwrap_or_default();
            if !system.contains(INGEST_MARKER) {
                continue;
            }
            let user = request["prompt"].as_str().unwrap_or_default().to_owned();
            let locale = system
                .rsplit_once(LANGUAGE_HEADING)
                .map(|(_, tail)| tail.trim_end().to_owned())
                .unwrap_or_default();
            out.push(Record {
                ts: row["ts"].as_str().unwrap_or_default().to_owned(),
                locale,
                author_is_assistant: user.contains("\nauthor: assistant\n"),
                has_attachments: user.contains("\nattachments:\n"),
                lossy_images: request["images"].as_array().is_some_and(|a| !a.is_empty()),
                sender: user
                    .strip_prefix("sender_id: ")
                    .and_then(|rest| rest.split('\n').next())
                    .map_or_else(String::new, |id| format!("user:{}", id.trim())),
                user,
            });
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

// ---------- Variants ----------

/// What to do to the bundled prompt before replaying it.
enum Variant {
    /// Send today's bundled prompt verbatim. The reference, and the
    /// noise-floor run.
    Baseline,
    /// Roadmap 49e: drop the sections this turn provably cannot reach —
    /// Part 12 on a user-authored turn, Part 9 on a turn with no media.
    Gated,
    /// Roadmap 49f: drop these sections unconditionally, to measure whether
    /// the prompt still decides the same way without them.
    Drop(Vec<String>),
}

impl Variant {
    /// Parse `baseline` / `gated` / `drop:<heading>;<heading>`. The drop
    /// arguments are matched as **prefixes of the `## ` heading line**, so
    /// `drop:Part 10;Part 11` is enough to name two sections.
    fn parse(spec: &str) -> Result<Self, String> {
        match spec {
            "baseline" => Ok(Self::Baseline),
            "gated" => Ok(Self::Gated),
            other => match other.strip_prefix("drop:") {
                Some(list) => {
                    let heads: Vec<String> = list
                        .split(';')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if heads.is_empty() {
                        return Err("`drop:` needs at least one heading".to_owned());
                    }
                    Ok(Self::Drop(heads))
                },
                None => Err(format!(
                    "unknown variant `{other}` (expected baseline | gated | drop:<heading>;…)"
                )),
            },
        }
    }

    /// The headings to remove for this record.
    fn sections_to_drop(&self, record: &Record) -> Vec<String> {
        match self {
            Self::Baseline => Vec::new(),
            Self::Gated => {
                let mut drop = Vec::new();
                if !record.author_is_assistant {
                    drop.push("Part 12".to_owned());
                }
                if !record.has_attachments {
                    drop.push("Part 9".to_owned());
                }
                drop
            },
            Self::Drop(heads) => heads.clone(),
        }
    }
}

/// Remove the `## ` section whose heading starts with `heading_prefix`,
/// from its own heading line up to (not including) the next `## ` heading.
///
/// Returns the body unchanged when the heading is not found — the caller
/// reports that as a miss rather than silently measuring a no-op.
fn drop_section(body: &str, heading_prefix: &str) -> Option<String> {
    let needle = format!("## {heading_prefix}");
    let start = body
        .match_indices(&needle)
        .find(|(i, _)| *i == 0 || body.as_bytes()[i - 1] == b'\n')
        .map(|(i, _)| i)?;
    let rest = &body[start + needle.len()..];
    let end = rest
        .match_indices("\n## ")
        .next()
        .map_or(body.len(), |(i, _)| start + needle.len() + i + 1);
    let mut out = String::with_capacity(body.len());
    out.push_str(&body[..start]);
    out.push_str(&body[end..]);
    Some(out)
}

/// Build the system prompt this record will be replayed with.
fn system_for(base: &str, record: &Record, variant: &Variant) -> Result<String, String> {
    let mut body = prompts::substitute(base, &[("locale", record.locale.as_str())]);
    for head in variant.sections_to_drop(record) {
        body = drop_section(&body, &head)
            .ok_or_else(|| format!("section `## {head}` not found in the bundled prompt"))?;
    }
    Ok(body)
}

// ---------- Replay ----------

/// One replayed answer, one line of the result file.
fn result_line(record: &Record, system_len: usize, text: &str, usage: &Value) -> String {
    let plan: Option<Value> = first_json_object(text);
    json!({
        "ts": record.ts,
        "system_len": system_len,
        "author_is_assistant": record.author_is_assistant,
        "has_attachments": record.has_attachments,
        "lossy_images": record.lossy_images,
        "sender": record.sender,
        "usage": usage,
        "parsed": plan.is_some(),
        "plan": plan,
    })
    .to_string()
}

/// Locate the outermost balanced `{ … }` in `raw` and parse it — the same
/// tolerance `ingest.rs` applies to the model's answer, reimplemented here
/// so the harness stays decoupled from the crate's private types.
fn first_json_object(raw: &str) -> Option<Value> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0_usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&raw[start..=i]).ok();
                }
            },
            _ => {},
        }
    }
    None
}

/// Run one completion, retrying the retriable failures with exponential
/// back-off. Rate limits and transport blips are expected at replay
/// concurrency; `Invalid` and `Auth` are the operator's problem and fail
/// immediately rather than burning four more attempts.
async fn complete_with_retry(
    backend: &dyn LlmBackend,
    request: CompletionRequest,
) -> Result<mwe_core::llm::CompletionResponse, LlmError> {
    let mut attempt = 1;
    loop {
        match backend.complete(request.clone()).await {
            Ok(response) => return Ok(response),
            Err(e @ (LlmError::Invalid(_) | LlmError::Auth(_))) => return Err(e),
            Err(e) if attempt >= MAX_ATTEMPTS => return Err(e),
            Err(_) => {
                let wait = BACKOFF_BASE_MS << (attempt - 1);
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                attempt += 1;
            },
        }
    }
}

/// Timestamps already present in `out`, so a re-run resumes instead of
/// re-buying answers we have.
fn already_done(out: &Path) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    if let Ok(text) = fs::read_to_string(out) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(row) = serde_json::from_str::<Value>(line)
                && let Some(ts) = row.get("ts").and_then(Value::as_str)
            {
                seen.insert(ts.to_owned());
            }
        }
    }
    seen
}

/// Build the backend named by `--backend`, and say out loud which meter the
/// run is about to spin.
fn build_backend(
    backend_kind: &str,
    model: &str,
    reasoning_effort: Option<&str>,
) -> Result<Arc<dyn LlmBackend>, String> {
    let backend: Arc<dyn LlmBackend> = match backend_kind {
        "gemini" => {
            let key = std::env::var(API_KEY_ENV)
                .map_err(|_| format!("{API_KEY_ENV} is not set — a replay needs a live key"))?;
            Arc::new(
                GeminiBackend::new(
                    GeminiApiKey::new(key).map_err(|e| format!("bad {API_KEY_ENV}: {e}"))?,
                    model,
                    API_KEY_ENV,
                )
                .map_err(|e| format!("cannot build the Gemini backend: {e}"))?
                // `thinkingLevel`. Production leaves the ingest slot at the
                // default `minimal`; raising it here is how we ask whether more
                // deliberation buys back the reproducibility the mandated
                // temperature costs us.
                .with_reasoning_effort(reasoning_effort),
            )
        },
        "anthropic" => {
            if reasoning_effort.is_some() {
                return Err(
                    "`--reasoning-effort` is a Gemini `thinkingLevel` knob; it has no \
                     effect on `--backend anthropic`"
                        .to_owned(),
                );
            }
            let key = std::env::var(ANTHROPIC_KEY_ENV).map_err(|_| {
                format!("{ANTHROPIC_KEY_ENV} is not set — a replay needs a live key")
            })?;
            eprintln!(
                "  credential: {}",
                if looks_like_subscription_token(&key) {
                    "subscription token (Bearer / Claude Code identity) — consumes plan quota"
                } else {
                    "Console API key — THIS RUN BILLS PER TOKEN"
                }
            );
            Arc::new(
                AnthropicBackend::new(
                    AnthropicApiKey::new(key)
                        .map_err(|e| format!("bad {ANTHROPIC_KEY_ENV}: {e}"))?,
                    model,
                    ANTHROPIC_KEY_ENV,
                )
                .map_err(|e| format!("cannot build the Anthropic backend: {e}"))?,
            )
        },
        other => {
            return Err(format!(
                "unknown `--backend {other}` — expected `gemini` or `anthropic`"
            ));
        },
    };
    Ok(backend)
}

/// Replay `records` under `variant` and stream the results into `out`.
async fn replay(
    records: &[Record],
    base: &str,
    variant: &Variant,
    backend_kind: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    concurrency: usize,
    out: &Path,
) -> Result<(), String> {
    let backend = build_backend(backend_kind, model, reasoning_effort)?;

    // Resume: a record already answered in `out` is not bought twice. A
    // rate-limited run is re-runnable for the cost of what it missed.
    let done_already = already_done(out);
    if !done_already.is_empty() {
        eprintln!(
            "  resuming: {} of {} already in {}",
            done_already.len(),
            records.len(),
            out.display()
        );
    }

    // Group records by the system prompt they will send, so identical
    // prefixes arrive back-to-back and the provider's implicit prefix cache
    // actually hits. Purely an economy of the harness — it cannot change a
    // decision, only the bill.
    let mut prepared: Vec<(usize, String)> = Vec::with_capacity(records.len());
    for (i, r) in records.iter().enumerate() {
        if done_already.contains(&r.ts) {
            continue;
        }
        prepared.push((i, system_for(base, r, variant)?));
    }
    prepared.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| a.1.cmp(&b.1)));

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
        .map_err(|e| format!("cannot open {}: {e}", out.display()))?;
    let mut set = tokio::task::JoinSet::new();
    let total = prepared.len();
    let mut queue = prepared.into_iter();
    let mut done = 0_usize;
    let mut failed = 0_usize;

    loop {
        while set.len() < concurrency.max(1) {
            let Some((idx, system)) = queue.next() else {
                break;
            };
            let backend = Arc::clone(&backend);
            let user = records[idx].user.clone();
            set.spawn(async move {
                let request = CompletionRequest::new(user)
                    .with_system(system.clone())
                    .with_temperature(REPLAY_TEMPERATURE)
                    .with_max_tokens(REPLAY_MAX_TOKENS);
                let outcome = complete_with_retry(backend.as_ref(), request).await;
                (idx, system.len(), outcome)
            });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (idx, system_len, outcome) =
            joined.map_err(|e| format!("replay task panicked: {e}"))?;
        match outcome {
            Ok(response) => {
                let usage = json!({
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                    "cached_prompt_tokens": response.usage.cached_prompt_tokens,
                });
                let line = result_line(&records[idx], system_len, &response.text, &usage);
                writeln!(file, "{line}")
                    .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
            },
            Err(e) => {
                failed += 1;
                eprintln!("  ! {} failed: {e}", records[idx].ts);
            },
        }
        done += 1;
        if done.is_multiple_of(25) || done == total {
            eprintln!("  … {done}/{total} replayed ({failed} failed)");
        }
    }
    println!(
        "wrote {} ({} replayed, {failed} failed)",
        out.display(),
        done - failed
    );
    Ok(())
}

// ---------- Comparison ----------

/// Turn-level fields compared verbatim. Free-text fields (`suggested_seed`)
/// are excluded — they differ on every sample and say nothing about the
/// decision.
const TURN_FIELDS: &[&str] = &["intent", "needs_disambig", "needs_project_docs"];

/// Per-extraction fields compared verbatim. Prose and free tags (`body`,
/// `page_description`, `topics`, `valid_from`) are excluded for the same
/// reason: they are generated language, not a decision.
const EXTRACTION_FIELDS: &[&str] = &[
    "target_wiki_id",
    "target_page",
    "owner_id",
    "allow_ids",
    "fact_type",
    "style",
    "requested_container",
    "salience",
    "engine_rule",
    "behaviour_rule",
    "behaviour_scope",
    "supersede_target",
    "attachments",
];

/// The subset of the comparison that is *consequential*: what the turn was
/// read as, how many facts came out, where each one is filed, whose it is
/// and who may read it. The rest of the compared surface — the page name,
/// the prose style, the `fact_type` hint — is a soft choice the engine
/// tolerates. Reported separately because the two rates answer different
/// questions: the wide one measures reproducibility, the core one measures
/// whether memory lands in the right place with the right audience.
const CORE_TURN_FIELDS: &[&str] = &["intent"];

/// Per-extraction half of the consequential subset. `owner_id` and
/// `allow_ids` are **deliberately absent**: comparing them raw counts a
/// change of *spelling* as a change of *decision*. Read access is
/// `owner ∪ allow ∪ sender` — all three, none of them sufficient alone — so
/// the only honest comparison is the reader set those three produce
/// together, which [`audience`] computes and [`core_agrees`] compares.
const CORE_EXTRACTION_FIELDS: &[&str] = &["target_wiki_id"];

/// The set of principals that can read a fact: `owner ∪ allow ∪ sender`,
/// per `acl::can_read`. Group principals are **not** expanded to their
/// members — that needs the enrollment tables, and it is unnecessary here
/// because both sides of a differential replay the *same* turn, so they
/// share a sender and the same group definitions. What this does collapse
/// is the dominant equivalence in production: `owner=group:g, allow=[]`
/// and `owner=user:<sender>, allow=[group:g]` are the same audience, and
/// raw-field comparison scored them as a disagreement.
fn audience(extraction: &Value, sender: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    if let Some(owner) = extraction.get("owner_id").and_then(Value::as_str) {
        set.insert(owner.to_owned());
    }
    if let Some(allow) = extraction.get("allow_ids").and_then(Value::as_array) {
        set.extend(
            allow
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned),
        );
    }
    if !sender.is_empty() {
        set.insert(sender.to_owned());
    }
    set.into_iter().collect()
}

/// Array-valued turn fields where only the length is a decision.
const ARRAY_FIELDS: &[&str] = &[
    "extractions",
    "closures",
    "closure_topics",
    "validity_edits",
    "acl_changes",
];

/// Read a result file into `ts -> line`.
fn load_results(path: &Path) -> Result<BTreeMap<String, Value>, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let row: Value = serde_json::from_str(line)
            .map_err(|e| format!("malformed line in {}: {e}", path.display()))?;
        if let Some(ts) = row.get("ts").and_then(Value::as_str) {
            out.insert(ts.to_owned(), row);
        }
    }
    Ok(out)
}

/// Fields whose absence means `false` — the model routinely omits a flag
/// it is not setting, and `absent` must not read as a different decision
/// from an explicit `false`. Getting this wrong inflates every diff by
/// tens of percent, all of it noise.
const DEFAULT_FALSE: &[&str] = &[
    "needs_disambig",
    "needs_project_docs",
    "requested_container",
    "engine_rule",
    "behaviour_rule",
];

/// Fields whose absence means the empty list, for the same reason.
const DEFAULT_EMPTY_LIST: &[&str] = &["allow_ids", "attachments"];

/// Canonicalise one field before comparison: an absent field becomes its
/// schema default, and set-valued fields lose their member order.
fn normalised(field: &str, value: Option<&Value>) -> Value {
    let value = value.cloned().unwrap_or(Value::Null);
    if value.is_null() {
        if DEFAULT_FALSE.contains(&field) {
            return Value::Bool(false);
        }
        if DEFAULT_EMPTY_LIST.contains(&field) {
            return Value::Array(Vec::new());
        }
        return Value::Null;
    }
    if DEFAULT_EMPTY_LIST.contains(&field)
        && let Value::Array(mut items) = value
    {
        items.sort_by_key(ToString::to_string);
        return Value::Array(items);
    }
    value
}

/// Do the two plans agree on the consequential subset — intent, how many
/// facts, and each fact's wiki / owner / audience?
fn core_agrees(a: &Value, b: &Value, sender: &str) -> bool {
    for field in CORE_TURN_FIELDS {
        if normalised(field, a.get(field)) != normalised(field, b.get(field)) {
            return false;
        }
    }
    let empty = Vec::new();
    let ea = a
        .get("extractions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let eb = b
        .get("extractions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if ea.len() != eb.len() {
        return false;
    }
    for (xa, xb) in ea.iter().zip(eb.iter()) {
        for field in CORE_EXTRACTION_FIELDS {
            if normalised(field, xa.get(field)) != normalised(field, xb.get(field)) {
                return false;
            }
        }
        if audience(xa, sender) != audience(xb, sender) {
            return false;
        }
    }
    true
}

/// Collect every field on which two plans disagree.
fn disagreements(a: &Value, b: &Value, sender: &str) -> Vec<String> {
    let mut out = Vec::new();
    for field in TURN_FIELDS {
        if normalised(field, a.get(field)) != normalised(field, b.get(field)) {
            out.push((*field).to_owned());
        }
    }
    for field in ARRAY_FIELDS {
        let la = a.get(field).and_then(Value::as_array).map_or(0, Vec::len);
        let lb = b.get(field).and_then(Value::as_array).map_or(0, Vec::len);
        if la != lb {
            out.push(format!("{field}.len"));
        }
    }
    let empty = Vec::new();
    let ea = a
        .get("extractions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let eb = b
        .get("extractions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for (i, (xa, xb)) in ea.iter().zip(eb.iter()).enumerate() {
        if audience(xa, sender) != audience(xb, sender) {
            out.push(format!("extractions[{i}].AUDIENCE"));
        }
        for field in EXTRACTION_FIELDS {
            if normalised(field, xa.get(field)) != normalised(field, xb.get(field)) {
                out.push(format!("extractions[{i}].{field}"));
            }
        }
    }
    out
}

/// Everything one differential run accumulates.
#[derive(Default)]
struct Tally {
    /// Records present in both files.
    shared: usize,
    /// Records where either side's answer would not parse.
    unparsed: usize,
    /// Records whose replay lost image bytes.
    lossy: usize,
    /// Records agreeing on every compared field.
    identical: usize,
    /// Records agreeing on intent + fact count + wiki/owner/audience.
    core_same: usize,
    /// Records where at least one side captured something.
    capturing: usize,
    /// …of those, the ones that agree on the core.
    core_same_capturing: usize,
    /// Prompt / cached tokens on each side.
    tokens: [usize; 4],
    /// Disagreement counts by field.
    per_field: BTreeMap<String, usize>,
    /// Drifting records, in file order.
    drifted: Vec<(String, Vec<String>)>,
}

/// Accumulate the differential between two loaded result files.
fn tally(a: &BTreeMap<String, Value>, b: &BTreeMap<String, Value>) -> Tally {
    let mut t = Tally::default();
    for (ts, ra) in a {
        let Some(rb) = b.get(ts) else { continue };
        t.shared += 1;
        for (slot, row, key) in [
            (0, ra, "prompt_tokens"),
            (1, rb, "prompt_tokens"),
            (2, ra, "cached_prompt_tokens"),
            (3, rb, "cached_prompt_tokens"),
        ] {
            let n = row["usage"][key].as_u64().unwrap_or(0);
            t.tokens[slot] += usize::try_from(n).unwrap_or(0);
        }
        if ra["lossy_images"].as_bool() == Some(true) {
            t.lossy += 1;
        }
        let (Some(pa), Some(pb)) = (ra.get("plan"), rb.get("plan")) else {
            t.unparsed += 1;
            continue;
        };
        if pa.is_null() || pb.is_null() {
            t.unparsed += 1;
            continue;
        }
        // Split the rate by whether the turn produced anything. A turn both
        // runs read as `skip` agrees trivially — averaging it in flatters
        // the classifier, since ~43% of production turns are exactly that.
        let captured = [pa, pb].iter().any(|p| {
            p.get("extractions")
                .and_then(Value::as_array)
                .is_some_and(|x| !x.is_empty())
        });
        if captured {
            t.capturing += 1;
        }
        let sender = ra["sender"].as_str().unwrap_or_default();
        if core_agrees(pa, pb, sender) {
            t.core_same += 1;
            if captured {
                t.core_same_capturing += 1;
            }
        }
        let diff = disagreements(pa, pb, sender);
        if diff.is_empty() {
            t.identical += 1;
        } else {
            for field in &diff {
                // Collapse the extraction index so the histogram reads by
                // field, not by position.
                let key = field
                    .split_once(']')
                    .map_or_else(|| field.clone(), |(_, rest)| format!("extractions{rest}"));
                *t.per_field.entry(key).or_default() += 1;
            }
            t.drifted.push((ts.clone(), diff));
        }
    }
    t
}

/// Percentage helper that reports `0.0` rather than `NaN` on an empty
/// denominator.
fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

/// Print the differential report for two result files.
fn compare(left: &Path, right: &Path) -> Result<(), String> {
    let a = load_results(left)?;
    let b = load_results(right)?;
    let t = tally(&a, &b);
    if t.shared == 0 {
        return Err("the two result files share no timestamps".to_owned());
    }
    let Tally {
        shared: n,
        unparsed,
        lossy,
        identical,
        core_same,
        capturing,
        core_same_capturing,
        tokens: [tok_a, tok_b, cached_a, cached_b],
        per_field,
        drifted,
    } = t;

    let comparable = n - unparsed;
    println!("compared {}  vs  {}", left.display(), right.display());
    println!("  shared records      {n}");
    println!("  unparsable answer   {unparsed}");
    println!("  replayed text-only  {lossy}  (spool dropped the image bytes — read with care)");
    if comparable > 0 {
        println!(
            "  SAME DECISION       {identical}/{comparable}  ({:.1}%)   every compared field",
            pct(identical, comparable)
        );
        println!(
            "  SAME ROUTING+ACL    {core_same}/{comparable}  ({:.1}%)   intent, fact count, wiki, and the reader set (owner∪allow∪sender)",
            pct(core_same, comparable)
        );
        if capturing > 0 {
            println!(
                "    …of which turns that actually captured: {core_same_capturing}/{capturing}  ({:.1}%)",
                pct(core_same_capturing, capturing)
            );
        }
    }
    println!(
        "  prompt tokens       {tok_a} -> {tok_b}  ({:+.1}%)",
        if tok_a == 0 {
            0.0
        } else {
            100.0 * (tok_b as f64 - tok_a as f64) / tok_a as f64
        }
    );
    println!(
        "  of which cached     {:.1}% -> {:.1}%",
        pct(cached_a, tok_a),
        pct(cached_b, tok_b)
    );
    if !per_field.is_empty() {
        println!("\n  where they drift (records per field):");
        let mut rows: Vec<(&String, &usize)> = per_field.iter().collect();
        rows.sort_by(|x, y| y.1.cmp(x.1).then_with(|| x.0.cmp(y.0)));
        for (field, count) in rows {
            println!("    {count:4}  {field}");
        }
    }
    if !drifted.is_empty() {
        println!("\n  first drifting records (timestamps only — the spool holds recalled memory):");
        for (ts, diff) in drifted.iter().take(10) {
            let ts: &str = ts;
            println!("    {ts}  {}", diff.join(", "));
        }
    }
    Ok(())
}

// ---------- Entry point ----------

/// Parsed command line.
struct Args {
    spool: PathBuf,
    out: Option<PathBuf>,
    variant: String,
    limit: Option<usize>,
    backend: String,
    model: String,
    /// Replay against a prompt file on disk instead of the bundled
    /// `ingest.md` — the only way to make the *previous* prompt version the
    /// baseline once the bundled one already carries the change under test.
    prompt_file: Option<PathBuf>,
    reasoning_effort: Option<String>,
    concurrency: usize,
    dry_run: bool,
    compare: Option<(PathBuf, PathBuf)>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            spool: PathBuf::from("./work/training-spool"),
            out: None,
            variant: "baseline".to_owned(),
            limit: None,
            backend: "gemini".to_owned(),
            model: "gemini-3-flash-preview".to_owned(),
            prompt_file: None,
            reasoning_effort: None,
            concurrency: 4,
            dry_run: false,
            compare: None,
        }
    }
}

/// Parse the flags documented in the module header.
fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("`{flag}` needs a value"));
        match flag.as_str() {
            "--spool" => args.spool = PathBuf::from(value()?),
            "--out" => args.out = Some(PathBuf::from(value()?)),
            "--variant" => args.variant = value()?,
            "--backend" => args.backend = value()?,
            "--model" => args.model = value()?,
            "--prompt-file" => args.prompt_file = Some(PathBuf::from(value()?)),
            "--reasoning-effort" => args.reasoning_effort = Some(value()?),
            "--limit" => {
                args.limit = Some(value()?.parse().map_err(|_| "`--limit` wants a number")?);
            },
            "--concurrency" => {
                args.concurrency = value()?
                    .parse()
                    .map_err(|_| "`--concurrency` wants a number")?;
            },
            "--dry-run" => args.dry_run = true,
            "--compare" => {
                let left = PathBuf::from(value()?);
                let right = PathBuf::from(value()?);
                args.compare = Some((left, right));
            },
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    Ok(args)
}

/// Print what a real run would send, and stop.
fn dry_run(records: &[Record], base: &str, variant: &Variant) -> Result<(), String> {
    let (mut total, mut gated_total) = (0_usize, 0_usize);
    let mut shapes: BTreeMap<usize, usize> = BTreeMap::new();
    for r in records {
        let baseline = prompts::substitute(base, &[("locale", r.locale.as_str())]);
        let system = system_for(base, r, variant)?;
        total += baseline.len() + r.user.len();
        gated_total += system.len() + r.user.len();
        *shapes.entry(system.len()).or_default() += 1;
    }
    let lossy = records.iter().filter(|r| r.lossy_images).count();
    println!("dry run — nothing was sent");
    println!("  records                {}", records.len());
    println!("  replayed text-only     {lossy}  (image bytes are not in the spool)");
    println!("  distinct system shapes {}", shapes.len());
    println!(
        "  bytes baseline→variant {total} -> {gated_total}  ({:+.1}%)",
        if total == 0 {
            0.0
        } else {
            100.0 * (gated_total as f64 - total as f64) / total as f64
        }
    );
    println!(
        "  ≈ input tokens         {} (÷3.6 B/token, the ratio the spool shows)",
        gated_total / 36 * 10
    );
    for (len, count) in &shapes {
        println!("    {count:4} records at {len} B");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        },
    };
    if let Some((left, right)) = &args.compare {
        return match compare(left, right) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            },
        };
    }

    let result = async {
        // The system prompt under test: the bundled `ingest.md` by default,
        // or any prompt file on disk. The override is what makes a
        // *previous* version usable as the baseline — `git show <rev>:…` it
        // to a file and point here — once the bundled copy already carries
        // the edit being measured.
        let (source, base) = match &args.prompt_file {
            None => (
                "<bundled>".to_owned(),
                prompts::extract_fenced_text(BUNDLED_INGEST_PROMPT_MD, "ingest", "<bundled>")
                    .map_err(|e| format!("bundled ingest.md is malformed: {e}"))?,
            ),
            Some(path) => {
                let label = path.display().to_string();
                let raw =
                    fs::read_to_string(path).map_err(|e| format!("cannot read {label}: {e}"))?;
                let text = prompts::extract_fenced_text(&raw, "ingest", &label)
                    .map_err(|e| format!("{label} is malformed: {e}"))?;
                (label, text)
            },
        };
        let variant = Variant::parse(&args.variant)?;
        let mut records = load_records(&args.spool)?;
        if records.is_empty() {
            return Err(format!(
                "no ingest-classifier records under {}",
                args.spool.display()
            ));
        }
        if let Some(limit) = args.limit {
            let from = records.len().saturating_sub(limit);
            records.drain(..from);
        }
        eprintln!(
            "{} records, prompt `{}`, variant `{}`, backend `{}`, model `{}`, thinking `{}`",
            records.len(),
            source,
            args.variant,
            args.backend,
            args.model,
            args.reasoning_effort
                .as_deref()
                .unwrap_or("minimal (default)")
        );
        if args.dry_run {
            return dry_run(&records, &base, &variant);
        }
        let out = args
            .out
            .as_ref()
            .ok_or("a replay needs `--out <file>` (or pass `--dry-run`)")?;
        replay(
            &records,
            &base,
            &variant,
            &args.backend,
            &args.model,
            args.reasoning_effort.as_deref(),
            args.concurrency,
            out,
        )
        .await
    }
    .await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        },
    }
}
