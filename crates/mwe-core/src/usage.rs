// SPDX-License-Identifier: AGPL-3.0-or-later
//! LLM usage ledger — one row per model call, and the aggregates the
//! dashboard's **Usage & spend** page reads back.
//!
//! ## Why this exists
//!
//! Every completion already returns a [`crate::llm::CompletionUsage`],
//! so the tokens were being *counted* long before this module: they
//! were logged, and they were written into the training spool. What
//! did not exist was a way to **ask a question of them**. "What did
//! last month cost, and which slot spent it" was a grep over per-day
//! JSONL files that only exist if the operator enabled a feature built
//! for distillation datasets, and that hold every prompt verbatim —
//! including the recalled memory of every user the deployment serves.
//!
//! Nobody should have to accept that exposure, or that archaeology, to
//! learn what their own memory costs to run. So the ledger is a table:
//! ~120 bytes a call, no prompt text at all, always on.
//!
//! ## Where it hooks in
//!
//! [`maybe_wrap`] decorates the built backend inside
//! [`crate::config::LlmFunctionConfig::build_backend`] — the one seam
//! every slot and every transport (MCP ingest, REM cycle, dashboard
//! chat, document worker) is constructed through, the same seam
//! [`crate::training_spool::maybe_wrap`] uses. No call site knows this
//! module exists, and a slot added tomorrow is recorded without anyone
//! remembering to.
//!
//! Health probes are **not** recorded: `health_check` delegates to the
//! inner backend untouched, because a liveness ping is not usage.
//!
//! ## What a row can answer, and what it deliberately cannot
//!
//! The row carries the dimensions a spend question needs — slot,
//! backend, model, how it is paid for, which process made the call —
//! and none of the content. It cannot tell you *which user's turn* cost
//! what, and that is on purpose: attributing spend per person would
//! make an operator surface out of something that reads across every
//! wiki's ACL, and the number the operator actually needs is the
//! deployment's.
//!
//! ## Money is estimated, never observed
//!
//! No provider tells us what it charged. The cost column is
//! `tokens × a rate the operator configured` ([`crate::config::LlmPricingConfig`]) —
//! which means it is exactly as right as the price list, is `None` for
//! a model nobody priced, and is **zero by construction** for a slot on
//! a subscription or a local model. Tokens are counted in all three
//! cases: on a flat subscription they stop being money and stay the
//! measure of load, which is what tells you whether the plan will hold.
//!
//! ## The clean-month problem
//!
//! A month's numbers are worth nothing when experiments are mixed in,
//! and after the fact the two are indistinguishable — the founder's
//! actual complaint about the first bill. Two columns fix it going
//! forward, and both are recorded at the moment the call is made rather
//! than inferred later: [`UsageSource`] separates the live server from
//! an operator running a REM cycle or a recall evaluation by hand, and
//! `tag` (from `MWE_USAGE_TAG`) marks a whole process as a deliberate
//! experiment.
//!
//! See `docs/design-notes/llm-usage-ledger.md`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use sqlx::Row;
use sqlx::SqlitePool;

use crate::config::{LlmFunction, LlmPricingConfig};
use crate::llm::{
    ChatRequest, ChatResponse, CompletionRequest, CompletionResponse, CompletionUsage, LlmBackend,
    LlmError, Result,
};

/// Default number of days of ledger history kept.
///
/// Over a year on purpose: the first question anybody asks of a spend
/// page after twelve months is "how does this July compare with last
/// July", and a 365-day window answers it only until the day it does
/// not.
pub const DEFAULT_RETENTION_DAYS: i64 = 400;

/// Env var naming the current process's traffic as an experiment.
///
/// Set it to anything (`MWE_USAGE_TAG=prompt-ab-test`) and every call
/// the process makes carries that label, so the month it lands in can
/// be read with the experiments taken back out. Unset — the normal
/// case — leaves `tag` NULL.
pub const USAGE_TAG_ENV: &str = "MWE_USAGE_TAG";

/// How the tokens of one call are paid for.
///
/// Not derivable from the backend tag: the same provider is reached
/// three different ways, and only one of them costs money per token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    /// Metered against an API key — the only case where tokens are
    /// money.
    Api,
    /// Covered by a flat subscription (the reserved
    /// `api_key_env: claude-code` login). Tokens still counted: they
    /// are the load, and the load is what decides whether the flat
    /// plan holds.
    Subscription,
    /// A model running on this machine. Costs electricity and latency,
    /// not tokens.
    Local,
}

impl Billing {
    /// The DB token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Subscription => "subscription",
            Self::Local => "local",
        }
    }

    /// Does a call billed this way turn tokens into money?
    #[must_use]
    pub const fn is_metered(self) -> bool {
        matches!(self, Self::Api)
    }

    /// Parse a stored token. We control every write, so an unknown
    /// value only keeps the read path total — and it falls back to
    /// [`Self::Api`] so an unreadable row is counted as *costing*
    /// something rather than silently as free.
    #[must_use]
    pub fn from_db(s: &str) -> Self {
        match s {
            "subscription" => Self::Subscription,
            "local" => Self::Local,
            _ => Self::Api,
        }
    }
}

/// Which process made the call — the coarse half of the clean-month
/// separation, and the half nobody has to remember to set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// The running server: ordinary production traffic.
    Serve,
    /// `mwe-mcp rem run-cycle` / `run-light` / `run-compile` invoked by
    /// hand. The scheduled cycles inside a running server are
    /// [`Self::Serve`] — the distinction is *who asked*, not what ran.
    RemCli,
    /// `mwe-mcp recall eval` — a measurement run, never user traffic.
    EvalCli,
    /// Anything else that installed a ledger without saying what it is.
    Other,
}

impl UsageSource {
    /// The DB token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::RemCli => "rem-cli",
            Self::EvalCli => "eval-cli",
            Self::Other => "other",
        }
    }

    /// Is this the deployment's ordinary traffic, as opposed to an
    /// operator's hand-run?
    #[must_use]
    pub const fn is_production(self) -> bool {
        matches!(self, Self::Serve)
    }
}

/// Process-wide ledger handle, installed once by the server binary
/// after it opens the engine DB. Same idiom as
/// [`crate::training_spool::install_global`] and the OAuth login store
/// — the recording decorator is built deep inside `build_backend`,
/// where no pool is in scope.
static GLOBAL_LEDGER: OnceLock<Arc<UsageLedger>> = OnceLock::new();

/// Install the process-wide ledger (first call wins; idempotent, so a
/// subcommand that opens the DB twice cannot double-install).
pub fn install_global(ledger: Arc<UsageLedger>) {
    let _ = GLOBAL_LEDGER.set(ledger);
}

/// The process-wide ledger, if [`install_global`] has run.
#[must_use]
pub fn global() -> Option<Arc<UsageLedger>> {
    GLOBAL_LEDGER.get().cloned()
}

/// Wrap `inner` so every call through it lands in the ledger; passthrough
/// when no ledger is installed (library/embedded/test use).
#[must_use]
pub fn maybe_wrap(
    inner: Box<dyn LlmBackend>,
    function: LlmFunction,
    backend_tag: &str,
    billing: Billing,
) -> Box<dyn LlmBackend> {
    match global() {
        Some(ledger) => Box::new(RecordingBackend {
            inner,
            function,
            backend_tag: backend_tag.to_owned(),
            billing,
            ledger,
        }),
        None => inner,
    }
}

/// The ledger: the pool it writes to plus the two labels every row of
/// this process carries.
pub struct UsageLedger {
    pool: SqlitePool,
    source: UsageSource,
    tag: Option<String>,
    retention_days: i64,
    /// Days-since-epoch of the last retention sweep, so the sweep runs
    /// once a day on a server that never restarts without costing a
    /// `DELETE` per call.
    last_pruned_day: AtomicI64,
}

impl UsageLedger {
    /// Build a ledger over `pool`, labelling this process's calls.
    ///
    /// The tag is read from [`USAGE_TAG_ENV`] once, here, rather than
    /// per call: a process is an experiment or it is not, and re-reading
    /// the environment on the hot path would let it be half of each.
    #[must_use]
    pub fn new(pool: SqlitePool, source: UsageSource, retention_days: i64) -> Self {
        let tag = std::env::var(USAGE_TAG_ENV)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if let Some(t) = &tag {
            tracing::info!(
                tag = %t,
                source = source.as_str(),
                "usage ledger: this process's calls are tagged as an experiment"
            );
        }
        Self {
            pool,
            source,
            tag,
            retention_days,
            last_pruned_day: AtomicI64::new(i64::MIN),
        }
    }

    /// The experiment tag this process stamps, if any.
    #[must_use]
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// The source label this process stamps.
    #[must_use]
    pub const fn source(&self) -> UsageSource {
        self.source
    }

    /// Drop rows past the retention window, at most once per UTC day.
    ///
    /// Pruning on every insert would be a full-table `DELETE` scan per
    /// LLM call; pruning only at startup would let a server that runs
    /// for a year grow without limit. Once a day is both.
    async fn maybe_prune(&self, now: chrono::DateTime<chrono::Utc>) {
        if self.retention_days <= 0 {
            return;
        }
        // Days since the epoch — a monotone day counter, no calendar
        // arithmetic and no `Datelike` import needed.
        let today = now.timestamp().div_euclid(86_400);
        if self.last_pruned_day.swap(today, Ordering::Relaxed) == today {
            return;
        }
        let cutoff = (now - chrono::Duration::days(self.retention_days)).to_rfc3339();
        match sqlx::query("DELETE FROM llm_usage WHERE ts < ?")
            .bind(&cutoff)
            .execute(&self.pool)
            .await
        {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::info!(
                    rows = r.rows_affected(),
                    retention_days = self.retention_days,
                    "usage ledger: pruned rows past the retention window"
                );
            },
            Ok(_) => {},
            Err(e) => tracing::warn!(error = %e, "usage ledger: retention sweep failed"),
        }
    }
}

/// Coarse class of a failed call, for the `error` column.
///
/// The class, never the message: a provider's error text can quote the
/// prompt back, and the ledger holds no prompt text by design.
const fn error_class(e: &LlmError) -> &'static str {
    match e {
        LlmError::Invalid(_) => "invalid",
        LlmError::Transport(_) => "transport",
        LlmError::Backend(_) => "backend",
        LlmError::Protocol(_) => "protocol",
        LlmError::RateLimit(_) => "rate_limit",
        LlmError::Auth(_) => "auth",
    }
}

/// Decorator that records every call — successful or not — through the
/// wrapped backend.
///
/// Unlike the training spool, failures **are** recorded. A spool wants
/// pairs to learn from and a failure has no completion; a ledger wants
/// to explain a bill, and a provider that refuses a request often
/// refuses it again on the retry.
struct RecordingBackend {
    inner: Box<dyn LlmBackend>,
    function: LlmFunction,
    backend_tag: String,
    billing: Billing,
    ledger: Arc<UsageLedger>,
}

impl RecordingBackend {
    /// One `record` call for both outcomes, so the two paths cannot
    /// drift apart in what they stamp.
    async fn write(
        &self,
        kind: &str,
        started: Instant,
        usage: Option<&CompletionUsage>,
        error: Option<&str>,
    ) {
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let now = chrono::Utc::now();
        let res = sqlx::query(
            "INSERT INTO llm_usage
               (ts, slot, backend, model, kind, billing, source, tag,
                prompt_tokens, completion_tokens, cached_prompt_tokens,
                cache_write_tokens, latency_ms, error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now.to_rfc3339())
        .bind(self.function.yaml_key())
        .bind(&self.backend_tag)
        .bind(self.inner.model_id())
        .bind(kind)
        .bind(self.billing.as_str())
        .bind(self.ledger.source.as_str())
        .bind(self.ledger.tag.as_deref())
        .bind(usage.and_then(|u| u.prompt_tokens).map(i64::from))
        .bind(usage.and_then(|u| u.completion_tokens).map(i64::from))
        .bind(usage.and_then(|u| u.cached_prompt_tokens).map(i64::from))
        .bind(usage.and_then(|u| u.cache_write_tokens).map(i64::from))
        .bind(latency_ms)
        .bind(error)
        .execute(&self.ledger.pool)
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "usage ledger: insert failed — call not recorded");
            return;
        }
        self.ledger.maybe_prune(now).await;
    }
}

#[async_trait]
impl LlmBackend for RecordingBackend {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let started = Instant::now();
        match self.inner.complete(request).await {
            Ok(response) => {
                self.write("complete", started, Some(&response.usage), None)
                    .await;
                Ok(response)
            },
            Err(e) => {
                self.write("complete", started, None, Some(error_class(&e)))
                    .await;
                Err(e)
            },
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let started = Instant::now();
        match self.inner.chat(request).await {
            Ok(response) => {
                self.write("chat", started, Some(&response.usage), None)
                    .await;
                Ok(response)
            },
            Err(e) => {
                self.write("chat", started, None, Some(error_class(&e)))
                    .await;
                Err(e)
            },
        }
    }

    async fn health_check(&self) -> Result<()> {
        // A liveness ping is not usage, and recording it would put a
        // row in the ledger for every dashboard page that probes a
        // slot. Delegated untouched, same as the spool does.
        self.inner.health_check().await
    }
}

// ---------------------------------------------------------------------
// Reading it back
// ---------------------------------------------------------------------

/// One `GROUP BY` bucket: the finest grain the page ever needs.
///
/// Grouped at the grain where **the model is still known**, because the
/// model is what carries the price — a rollup by slot alone could never
/// be turned into money afterwards. Every view the page renders (by
/// slot, by model, by day, by month, cache share) is a fold over these
/// in memory, so the whole page is one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageBucket {
    /// `YYYY-MM-DD`, **UTC**. Provider billing periods are not local
    /// either, and a ledger that silently used the host's timezone
    /// would put a call at 01:00 Rome time in the previous day's total
    /// for one reader and not another.
    pub day: String,
    /// [`LlmFunction::yaml_key`].
    pub slot: String,
    /// Backend tag (`ollama` | `anthropic` | `gemini` | `openrouter`).
    pub backend: String,
    /// Model id as the backend reported it.
    pub model: String,
    /// [`Billing::as_str`].
    pub billing: String,
    /// [`UsageSource::as_str`].
    pub source: String,
    /// Experiment tag, `None` for ordinary traffic.
    pub tag: Option<String>,
    /// Calls in the bucket, failures included.
    pub calls: i64,
    /// Of which failed (no usage reported).
    pub failed: i64,
    /// Total prompt tokens — cache buckets **included**.
    pub prompt_tokens: i64,
    /// Prompt tokens served from the prefix cache (a subset of
    /// [`Self::prompt_tokens`]).
    pub cached_prompt_tokens: i64,
    /// Prompt tokens written into the prefix cache (a subset of
    /// [`Self::prompt_tokens`]).
    pub cache_write_tokens: i64,
    /// Tokens the model emitted.
    pub completion_tokens: i64,
    /// Summed latency, for a mean the page can divide out itself.
    pub latency_ms_total: i64,
}

impl UsageBucket {
    /// Prompt tokens billed at the **full** input rate: the total minus
    /// both cache buckets.
    ///
    /// Saturating rather than wrapping because the three columns come
    /// from a provider, not from us: a backend that ever reported a
    /// cache bucket larger than the prompt would produce a nonsense
    /// negative here and poison every total downstream.
    #[must_use]
    pub const fn plain_prompt_tokens(&self) -> i64 {
        let left = self
            .prompt_tokens
            .saturating_sub(self.cached_prompt_tokens)
            .saturating_sub(self.cache_write_tokens);
        if left < 0 { 0 } else { left }
    }

    /// Every token the call moved, prompt and completion.
    #[must_use]
    pub const fn total_tokens(&self) -> i64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Estimated money for this bucket, in the price list's currency.
    ///
    /// `None` means *not priced*, which the page must render as "not
    /// priced" and never as `0.00` — the difference between "this was
    /// free" and "nobody told us what it costs" is the whole point of
    /// the surface. A non-metered bucket (subscription, local model) is
    /// `Some(0.0)`: that one really is zero.
    #[must_use]
    pub fn estimated_cost(&self, pricing: &LlmPricingConfig) -> Option<f64> {
        if !Billing::from_db(&self.billing).is_metered() {
            return Some(0.0);
        }
        let rate = pricing.rate_for(&self.model)?;
        #[allow(
            clippy::cast_precision_loss,
            reason = "token counts are far below 2^53; this is a money estimate, not accounting"
        )]
        let per_million = |tokens: i64, price: f64| (tokens as f64) * price / 1_000_000.0;
        Some(
            per_million(self.plain_prompt_tokens(), rate.input)
                + per_million(self.cached_prompt_tokens, rate.cached_input_rate())
                + per_million(self.cache_write_tokens, rate.cache_write_rate())
                + per_million(self.completion_tokens, rate.output),
        )
    }
}

/// Read the ledger, grouped, from `since_day` (inclusive, `YYYY-MM-DD`
/// UTC) onward. `None` reads the whole ledger.
///
/// # Errors
///
/// Propagates sqlx failures.
pub async fn buckets(
    pool: &SqlitePool,
    since_day: Option<&str>,
) -> crate::Result<Vec<UsageBucket>> {
    // `substr(ts, 1, 10)` is the day: `ts` is RFC-3339 UTC, so the
    // first ten characters are `YYYY-MM-DD` and string comparison over
    // them orders exactly as time does.
    let sql = "SELECT substr(ts, 1, 10) AS day, slot, backend, model, billing, source, tag,
                      COUNT(*)                                  AS calls,
                      SUM(CASE WHEN error IS NULL THEN 0 ELSE 1 END) AS failed,
                      COALESCE(SUM(prompt_tokens), 0)           AS prompt_tokens,
                      COALESCE(SUM(cached_prompt_tokens), 0)    AS cached_prompt_tokens,
                      COALESCE(SUM(cache_write_tokens), 0)      AS cache_write_tokens,
                      COALESCE(SUM(completion_tokens), 0)       AS completion_tokens,
                      COALESCE(SUM(latency_ms), 0)              AS latency_ms_total
               FROM llm_usage
               WHERE (?1 IS NULL OR substr(ts, 1, 10) >= ?1)
               GROUP BY day, slot, backend, model, billing, source, tag
               ORDER BY day DESC, slot ASC";
    let rows = sqlx::query(sql)
        .bind(since_day)
        .fetch_all(pool)
        .await
        .map_err(|e| crate::Error::Other(format!("usage buckets: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| UsageBucket {
            day: r.get("day"),
            slot: r.get("slot"),
            backend: r.get("backend"),
            model: r.get("model"),
            billing: r.get("billing"),
            source: r.get("source"),
            tag: r.get("tag"),
            calls: r.get("calls"),
            failed: r.get("failed"),
            prompt_tokens: r.get("prompt_tokens"),
            cached_prompt_tokens: r.get("cached_prompt_tokens"),
            cache_write_tokens: r.get("cache_write_tokens"),
            completion_tokens: r.get("completion_tokens"),
            latency_ms_total: r.get("latency_ms_total"),
        })
        .collect())
}

/// The oldest day the ledger still holds (`YYYY-MM-DD` UTC), or `None`
/// when it is empty.
///
/// The page prints it verbatim: a total is unreadable without knowing
/// how far back it reaches, and "since we started recording" is a
/// different sentence from "since the retention window began".
///
/// # Errors
///
/// Propagates sqlx failures.
pub async fn first_day(pool: &SqlitePool) -> crate::Result<Option<String>> {
    let row = sqlx::query("SELECT MIN(substr(ts, 1, 10)) AS d FROM llm_usage")
        .fetch_one(pool)
        .await
        .map_err(|e| crate::Error::Other(format!("usage first_day: {e}")))?;
    Ok(row.get::<Option<String>, _>("d"))
}

/// Sum a slice of buckets into one.
///
/// The page folds the same slice several ways (by slot, by model, by
/// day); this is the fold, and it keeps the dimension fields of the
/// first bucket so a caller that grouped by one axis still has that
/// axis's value on the result.
#[must_use]
pub fn fold(buckets: &[UsageBucket]) -> UsageBucket {
    let mut out = buckets.first().cloned().unwrap_or(UsageBucket {
        day: String::new(),
        slot: String::new(),
        backend: String::new(),
        model: String::new(),
        billing: String::new(),
        source: String::new(),
        tag: None,
        calls: 0,
        failed: 0,
        prompt_tokens: 0,
        cached_prompt_tokens: 0,
        cache_write_tokens: 0,
        completion_tokens: 0,
        latency_ms_total: 0,
    });
    out.calls = 0;
    out.failed = 0;
    out.prompt_tokens = 0;
    out.cached_prompt_tokens = 0;
    out.cache_write_tokens = 0;
    out.completion_tokens = 0;
    out.latency_ms_total = 0;
    for b in buckets {
        out.calls += b.calls;
        out.failed += b.failed;
        out.prompt_tokens += b.prompt_tokens;
        out.cached_prompt_tokens += b.cached_prompt_tokens;
        out.cache_write_tokens += b.cache_write_tokens;
        out.completion_tokens += b.completion_tokens;
        out.latency_ms_total += b.latency_ms_total;
    }
    out
}

/// Total estimated money over a slice, plus how much of it is missing a
/// price.
///
/// Returned together on purpose: a total with unpriced calls hidden
/// behind it is the one number on this page that could mislead, so the
/// caller cannot render the sum without also holding the count of what
/// the sum left out.
#[must_use]
pub fn total_cost(buckets: &[UsageBucket], pricing: &LlmPricingConfig) -> (f64, i64) {
    let mut total = 0.0;
    let mut unpriced_calls = 0;
    for b in buckets {
        match b.estimated_cost(pricing) {
            Some(c) => total += c,
            None => unpriced_calls += b.calls,
        }
    }
    (total, unpriced_calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelPrice;
    use crate::llm::FakeLlmBackend;

    async fn pool_with_ledger(source: UsageSource) -> (tempfile::TempDir, Arc<UsageLedger>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let ledger = Arc::new(UsageLedger::new(pool, source, DEFAULT_RETENTION_DAYS));
        (dir, ledger)
    }

    /// A backend that refuses everything, with a message that quotes a
    /// prompt back — the shape of a real provider refusal, and the
    /// reason the ledger stores the *class* and not the text.
    struct AlwaysFails;

    #[async_trait]
    impl LlmBackend for AlwaysFails {
        fn model_id(&self) -> &'static str {
            "fake-model"
        }
        async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
            Err(LlmError::RateLimit(
                "slow down — your prompt began \"write the page\"".to_owned(),
            ))
        }
        async fn chat(&self, _r: ChatRequest) -> Result<ChatResponse> {
            Err(LlmError::RateLimit("slow down".to_owned()))
        }
        async fn health_check(&self) -> Result<()> {
            Ok(())
        }
    }

    fn wrap(ledger: &Arc<UsageLedger>, billing: Billing) -> RecordingBackend {
        RecordingBackend {
            inner: Box::new(FakeLlmBackend::new("fake-model", "hello")),
            function: LlmFunction::Ingest,
            backend_tag: "anthropic".to_owned(),
            billing,
            ledger: Arc::clone(ledger),
        }
    }

    #[tokio::test]
    async fn a_call_lands_as_one_row_with_its_labels() {
        let (_dir, ledger) = pool_with_ledger(UsageSource::Serve).await;
        let backend = wrap(&ledger, Billing::Api);
        backend
            .complete(CompletionRequest::new("classify this"))
            .await
            .expect("complete");

        let rows = buckets(&ledger.pool, None).await.expect("buckets");
        assert_eq!(rows.len(), 1);
        let b = &rows[0];
        assert_eq!(b.slot, "ingest");
        assert_eq!(b.backend, "anthropic");
        assert_eq!(b.model, "fake-model");
        assert_eq!(b.billing, "api");
        assert_eq!(b.source, "serve");
        assert_eq!(b.tag, None);
        assert_eq!(b.calls, 1);
        assert_eq!(b.failed, 0);
        // The fake reports a word count as the prompt and nothing else,
        // which is enough to prove the usage bag was read rather than
        // defaulted away.
        assert_eq!(b.prompt_tokens, 2);
    }

    /// The spool records only successes, because a failure has no pair
    /// to learn from. The ledger must record failures too — a refused
    /// request is exactly the thing that shows up on a bill as a retry
    /// nobody remembers making.
    #[tokio::test]
    async fn a_failed_call_is_recorded_with_its_class_and_no_tokens() {
        let (_dir, ledger) = pool_with_ledger(UsageSource::Serve).await;
        let backend = RecordingBackend {
            inner: Box::new(AlwaysFails),
            function: LlmFunction::Cronista,
            backend_tag: "anthropic".to_owned(),
            billing: Billing::Api,
            ledger: Arc::clone(&ledger),
        };
        backend
            .complete(CompletionRequest::new("write the page"))
            .await
            .expect_err("must propagate");

        let row = sqlx::query("SELECT error, prompt_tokens FROM llm_usage")
            .fetch_one(&ledger.pool)
            .await
            .expect("row");
        // The class, never the message: the provider's text can quote
        // the prompt back, and no prompt text belongs in this table.
        assert_eq!(row.get::<String, _>("error"), "rate_limit");
        assert!(
            !row.get::<String, _>("error").contains("write the page"),
            "the provider's message must not reach the ledger"
        );
        assert_eq!(row.get::<Option<i64>, _>("prompt_tokens"), None);

        let rows = buckets(&ledger.pool, None).await.expect("buckets");
        assert_eq!(rows[0].calls, 1);
        assert_eq!(rows[0].failed, 1);
    }

    /// A liveness probe is not usage. The dashboard pings every slot on
    /// every load of the LLM page, and a row per ping would drown the
    /// ledger in calls nobody made.
    #[tokio::test]
    async fn a_health_probe_is_not_recorded() {
        let (_dir, ledger) = pool_with_ledger(UsageSource::Serve).await;
        let backend = wrap(&ledger, Billing::Api);
        backend.health_check().await.expect("health");
        assert!(
            buckets(&ledger.pool, None)
                .await
                .expect("buckets")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_experiment_tag_rides_every_call_of_a_tagged_process() {
        let (_dir, ledger) = pool_with_ledger(UsageSource::Serve).await;
        // `UsageLedger::new` reads the env once; build a tagged one by
        // hand rather than mutating the process environment, which
        // other tests in this binary share.
        let tagged = Arc::new(UsageLedger {
            pool: ledger.pool.clone(),
            source: UsageSource::EvalCli,
            tag: Some("prompt-ab".to_owned()),
            retention_days: DEFAULT_RETENTION_DAYS,
            last_pruned_day: AtomicI64::new(i64::MIN),
        });
        wrap(&tagged, Billing::Api)
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");
        wrap(&ledger, Billing::Api)
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");

        let rows = buckets(&ledger.pool, None).await.expect("buckets");
        assert_eq!(rows.len(), 2, "tagged and untagged must not be one bucket");
        let tags: Vec<Option<String>> = rows.iter().map(|r| r.tag.clone()).collect();
        assert!(tags.contains(&Some("prompt-ab".to_owned())));
        assert!(tags.contains(&None));
        // And the source separates them without anybody setting a tag.
        let sources: Vec<&str> = rows.iter().map(|r| r.source.as_str()).collect();
        assert!(sources.contains(&"eval-cli") && sources.contains(&"serve"));
    }

    fn priced(
        model: &str,
        input: f64,
        cached: Option<f64>,
        write: Option<f64>,
        output: f64,
    ) -> LlmPricingConfig {
        LlmPricingConfig {
            currency: Some("USD".to_owned()),
            models: vec![ModelPrice {
                model: model.to_owned(),
                input,
                cached_input: cached,
                cache_write: write,
                output,
            }],
            extra: serde_yaml::Mapping::new(),
        }
    }

    fn bucket_with(
        model: &str,
        prompt: i64,
        cached: i64,
        write: i64,
        completion: i64,
    ) -> UsageBucket {
        UsageBucket {
            day: "2026-07-29".to_owned(),
            slot: "cronista".to_owned(),
            backend: "anthropic".to_owned(),
            model: model.to_owned(),
            billing: "api".to_owned(),
            source: "serve".to_owned(),
            tag: None,
            calls: 1,
            failed: 0,
            prompt_tokens: prompt,
            cached_prompt_tokens: cached,
            cache_write_tokens: write,
            completion_tokens: completion,
            latency_ms_total: 1_000,
        }
    }

    /// The three prompt buckets must be priced at three rates. Pricing
    /// the whole prompt at the input rate — the obvious shortcut — is
    /// what makes a cached workload look four times more expensive than
    /// it is, which is the complaint this page exists to answer.
    #[test]
    fn each_prompt_bucket_is_priced_at_its_own_rate() {
        let pricing = priced("claude-x", 3.0, Some(0.30), Some(3.75), 15.0);
        // 1M prompt tokens: 100k plain, 800k read from cache, 100k written.
        let b = bucket_with("claude-x", 1_000_000, 800_000, 100_000, 200_000);
        let cost = b.estimated_cost(&pricing).expect("priced");
        // 0.1*3 + 0.8*0.30 + 0.1*3.75 + 0.2*15 = 0.3 + 0.24 + 0.375 + 3.0
        assert!((cost - 3.915).abs() < 1e-9, "got {cost}");
        // The naive reading — everything at the input rate — would be
        // 3.0 + 3.0 = 6.0, more than half as much again.
        assert!(cost < 6.0);
    }

    /// An operator who lists a price but not the cache rates gets an
    /// **upper bound**, not a silent discount: an unstated rate falls
    /// back to the input rate rather than to zero.
    #[test]
    fn unstated_cache_rates_fall_back_to_the_input_rate() {
        let pricing = priced("claude-x", 3.0, None, None, 15.0);
        let b = bucket_with("claude-x", 1_000_000, 800_000, 100_000, 0);
        let cost = b.estimated_cost(&pricing).expect("priced");
        assert!((cost - 3.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn an_unpriced_model_is_unknown_not_free() {
        let pricing = priced("claude-x", 3.0, None, None, 15.0);
        let b = bucket_with("gemini-y", 1_000_000, 0, 0, 0);
        assert_eq!(b.estimated_cost(&pricing), None);
        let (total, unpriced) = total_cost(&[b], &pricing);
        assert!((total - 0.0).abs() < f64::EPSILON);
        assert_eq!(
            unpriced, 1,
            "the caller must be able to say what it left out"
        );
    }

    /// Tokens on a flat subscription are load, not money — and the zero
    /// must be a real zero, distinguishable from "nobody set a price".
    #[test]
    fn subscription_and_local_tokens_cost_nothing_even_when_priced() {
        let pricing = priced("claude-x", 3.0, None, None, 15.0);
        let mut b = bucket_with("claude-x", 1_000_000, 0, 0, 500_000);
        b.billing = "subscription".to_owned();
        assert_eq!(b.estimated_cost(&pricing), Some(0.0));
        b.billing = "local".to_owned();
        assert_eq!(b.estimated_cost(&pricing), Some(0.0));
        // …and the tokens are still counted.
        assert_eq!(b.total_tokens(), 1_500_000);
    }

    #[tokio::test]
    async fn the_window_filter_drops_older_days() {
        let (_dir, ledger) = pool_with_ledger(UsageSource::Serve).await;
        for (ts, slot) in [
            ("2026-05-01T10:00:00+00:00", "ingest"),
            ("2026-07-29T10:00:00+00:00", "cronista"),
        ] {
            sqlx::query(
                "INSERT INTO llm_usage (ts, slot, backend, model, kind, billing, source, prompt_tokens)
                 VALUES (?, ?, 'anthropic', 'm', 'complete', 'api', 'serve', 10)",
            )
            .bind(ts)
            .bind(slot)
            .execute(&ledger.pool)
            .await
            .expect("insert");
        }
        assert_eq!(buckets(&ledger.pool, None).await.expect("all").len(), 2);
        let recent = buckets(&ledger.pool, Some("2026-07-01"))
            .await
            .expect("window");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].slot, "cronista");
        assert_eq!(
            first_day(&ledger.pool).await.expect("first"),
            Some("2026-05-01".to_owned())
        );
    }

    #[test]
    fn folding_sums_the_counters_and_keeps_the_axis() {
        let a = bucket_with("m", 10, 1, 2, 3);
        let mut b = bucket_with("m", 20, 2, 4, 6);
        b.day = "2026-07-30".to_owned();
        let f = fold(&[a, b]);
        assert_eq!(f.prompt_tokens, 30);
        assert_eq!(f.cached_prompt_tokens, 3);
        assert_eq!(f.cache_write_tokens, 6);
        assert_eq!(f.completion_tokens, 9);
        assert_eq!(f.calls, 2);
        assert_eq!(f.slot, "cronista", "the grouped-on axis survives the fold");
        assert_eq!(f.plain_prompt_tokens(), 30 - 3 - 6);
    }

    #[test]
    fn folding_an_empty_slice_is_all_zeroes_not_a_panic() {
        let f = fold(&[]);
        assert_eq!(f.calls, 0);
        assert_eq!(f.total_tokens(), 0);
    }
}
