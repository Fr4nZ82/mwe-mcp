// SPDX-License-Identifier: AGPL-3.0-or-later
//! Daily budget — the ceiling that warns the operator, then stops
//! paid model calls until the next UTC day.
//!
//! ## What it is, and the one thing it is not
//!
//! An operator sets `budget.daily_limit` in the currency of their price
//! list. Today's spend is the same estimate the **Usage & spend** page
//! prints: the [`crate::usage`] ledger's tokens, priced by
//! [`crate::config::LlmPricingConfig`]. Cross the warn threshold and the
//! operator is told once, on the reverse channel and on the dashboard.
//! Reach the budget and every **metered** call is refused with
//! [`crate::llm::LlmError::Budget`] until midnight UTC, or until the
//! operator raises the budget or lifts the stop for the day.
//!
//! It is **not** a way to run this product without a model. The six
//! slots stay mandatory and stay configured; a stop is a spending
//! decision the operator took, it lasts until they take it back, and
//! every surface says so in those words. A deployment under a budget
//! stop is a deployment its owner deliberately paused, not a cheaper
//! tier of the product.
//!
//! ## What the stop actually covers
//!
//! Metered calls, and nothing else. A slot on a flat subscription or on
//! a model running on this machine costs no money per token
//! ([`crate::usage::Billing`]), contributes nothing to the day's spend,
//! and is never refused — the budget is about a bill, so it binds exactly
//! what lands on one.
//!
//! Every slot is covered, the dashboard's `operator_chat` included.
//! There is no exempt slot, because the operator does not need one: the
//! budget is raised, and the day is unlocked, from a form on the Usage &
//! spend page, which needs no model at all. An exemption would be a
//! second mechanism whose only effect is that the most expensive slot in
//! the deployment keeps spending after the operator said stop.
//!
//! Health probes are **not** refused. They are how an operator checks
//! that their models still answer, they carry a handful of tokens on
//! demand, and the ledger already holds that a probe is not usage.
//!
//! ## What the callers do about it
//!
//! Nothing new. The stop arrives as an ordinary [`crate::llm::LlmError`]
//! from the backend, so every path that already degrades on an
//! unreachable model degrades identically here: a user turn falls to
//! `intent=skip` with the canonical degraded seed and the recall the
//! engine gives without a model, the nightly cycle skips the round and
//! says why, the dashboard chat says it cannot answer. One mechanism,
//! and no call site knows this module exists.
//!
//! ## Why the ledger and not a counter
//!
//! The day's spend is recomputed from `llm_usage`, cached for a few
//! seconds (`SPEND_CACHE_TTL`). A running in-memory total would be cheaper and
//! wrong in three ways that all matter here: it starts at zero after a
//! restart, it misses the rows a hand-run `mwe-mcp rem run-cycle` writes
//! into the same day, and it cannot be reconciled with the page that
//! shows the operator the number. The consequence of the cache is worth
//! stating plainly: calls already in flight when the budget is crossed
//! complete, and the guard refuses the *next* one. A budget is a ceiling on
//! what gets started, not a guarantee about a bill.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock};

use crate::config::{BudgetConfig, LlmPricingConfig};
use crate::llm::{
    ChatRequest, ChatResponse, CompletionRequest, CompletionResponse, LlmBackend, LlmError, Result,
};
use crate::usage::{self, Billing};

/// Warn threshold applied when the operator sets a budget and says nothing
/// about warning: four fifths of the way there.
///
/// Late enough that an ordinary day does not trip it, early enough that
/// the remaining fifth is time to react in — which is the only property
/// that makes a warning worth sending at all.
pub const DEFAULT_WARN_AT_PERCENT: u8 = 80;

/// How long a computed day-spend is reused before the ledger is read
/// again.
///
/// Five seconds buys back one `GROUP BY` per call on a busy deployment
/// and costs at most five seconds of calls started after the budget was
/// crossed. See the module docs on why that trade is the honest one.
const SPEND_CACHE_TTL: Duration = Duration::from_secs(5);

/// `engine_meta` key holding the `YYYY-MM-DD` the operator lifted the
/// stop for. It is a **day**, not a flag, so the lift expires by itself
/// at midnight and cannot be left on by someone who forgot it.
const UNLOCKED_DAY_KEY: &str = "budget.unlocked_day";

/// `engine_meta` key prefix holding the `YYYY-MM-DD` a threshold's
/// notice was last sent for, one key per [`Threshold`] — the same shape
/// as the compile-failure notice, which also fires once per threshold
/// rather than once per condition.
const NOTIFIED_DAY_PREFIX: &str = "budget.notified_day.";

/// Which line today's spend crossed.
///
/// Two thresholds, one mechanism: the warning and the stop are the same
/// notice at two heights, so they are emitted, deduplicated and reported
/// by one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    /// `warn_at_percent` of the budget. Nothing stops; the operator is told
    /// while there is still headroom to decide in.
    Warn,
    /// The budget itself. Metered calls are refused for the rest of the UTC
    /// day.
    Stop,
}

impl Threshold {
    /// Wire-stable token, used in the event payload and in the
    /// `engine_meta` dedup key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Stop => "stop",
        }
    }
}

/// Today's spend against today's budget — everything the dashboard, the
/// notice and the gate each need, answered once.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetState {
    /// `YYYY-MM-DD`, UTC — the day these figures are for. UTC because
    /// the ledger is UTC and a provider's billing period is not local
    /// either.
    pub day: String,
    /// Estimated money spent today on metered calls, in
    /// [`Self::currency`].
    pub spent: f64,
    /// Calls today on a model with no configured rate. Their tokens are
    /// counted and their cost is not, so a budget read against a
    /// half-filled price list is looser than the operator thinks — which
    /// is a sentence the page has to be able to print.
    pub unpriced_calls: i64,
    /// The budget, or `None` when the operator set none.
    pub limit: Option<f64>,
    /// Fraction of the budget at which the warning fires.
    pub warn_fraction: f64,
    /// The operator lifted the stop for this day.
    pub unlocked: bool,
    /// Currency label from the price list, printed verbatim.
    pub currency: Option<String>,
}

impl BudgetState {
    /// Share of the budget already spent, `None` without a budget.
    fn fraction_used(&self) -> Option<f64> {
        self.limit.map(|l| self.spent / l)
    }

    /// The same as a whole percentage, for display.
    ///
    /// Rounded, not floored: 4.60 of 4.00 is 1.1499999… in binary
    /// floating point, and flooring it prints 114% beside the two
    /// figures it is computed from.
    #[must_use]
    pub fn percent_used(&self) -> Option<i64> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a display percentage; the value is bounded by the ratio it shows"
        )]
        self.fraction_used().map(|f| (f * 100.0).round() as i64)
    }

    /// Is spending stopped right now?
    ///
    /// The lift is checked here rather than at the budget, so an unlocked
    /// day still reports how far past the budget it went — the operator who
    /// lifted it is the one person who wants that number.
    #[must_use]
    pub fn stopped(&self) -> bool {
        !self.unlocked && self.limit.is_some_and(|l| self.spent >= l)
    }

    /// The highest threshold today's spend has crossed, ignoring the
    /// lift: a day the operator unlocked has still passed its budget, and
    /// the notice for that is worth sending once.
    #[must_use]
    pub fn crossed(&self) -> Option<Threshold> {
        let limit = self.limit?;
        if self.spent >= limit {
            Some(Threshold::Stop)
        } else if self.spent >= limit * self.warn_fraction {
            Some(Threshold::Warn)
        } else {
            None
        }
    }

    /// The sentence a refused call carries, and the one the dashboard
    /// prints. One wording, so the operator reading a log and the
    /// operator reading the page are told the same thing.
    #[must_use]
    pub fn stop_message(&self) -> String {
        let currency = self
            .currency
            .as_deref()
            .map(|c| format!(" {c}"))
            .unwrap_or_default();
        let limit = self.limit.unwrap_or(0.0);
        format!(
            "The daily budget for {} is spent ({:.2}{} of {:.2}{}). Paid model calls resume at \
             00:00 UTC, or when the operator raises the budget or unlocks the day from the \
             dashboard's Usage & spend page.",
            self.day, self.spent, currency, limit, currency
        )
    }
}

/// Process-wide guard, installed once by the server binary after it
/// opens the engine DB.
///
/// Same idiom as [`crate::usage::install_global`], and for the same
/// reason: the gate decorator is built deep inside
/// [`crate::config::LlmFunctionConfig::build_backend`], where no pool is
/// in scope.
static GLOBAL_GUARD: OnceLock<Arc<BudgetGuard>> = OnceLock::new();

/// Install the process-wide guard (first call wins; idempotent).
pub fn install_global(guard: Arc<BudgetGuard>) {
    let _ = GLOBAL_GUARD.set(guard);
}

/// The process-wide guard, if [`install_global`] has run.
#[must_use]
pub fn global() -> Option<Arc<BudgetGuard>> {
    GLOBAL_GUARD.get().cloned()
}

/// Wrap `inner` so a metered call is refused while the budget is reached;
/// passthrough for a call that costs no money, and passthrough when no
/// guard is installed (library / embedded / test use).
#[must_use]
pub fn maybe_gate(inner: Box<dyn LlmBackend>, billing: Billing) -> Box<dyn LlmBackend> {
    if !billing.is_metered() {
        return inner;
    }
    match global() {
        Some(guard) => Box::new(GatedBackend { inner, guard }),
        None => inner,
    }
}

/// The guard: the ledger it reads, the two config sections it reads it
/// against, and the memo that keeps it from reading it on every call.
pub struct BudgetGuard {
    pool: SqlitePool,
    config: RwLock<BudgetConfig>,
    pricing: RwLock<LlmPricingConfig>,
    cached: Mutex<Option<(Instant, BudgetState)>>,
    /// Set once a `stopped` state has been logged, cleared when spending
    /// resumes, so a stopped deployment writes one log line instead of
    /// one per refused call.
    stop_logged: AtomicBool,
}

impl BudgetGuard {
    /// Build a guard over `pool`, reading the budget and the price list an
    /// operator has configured.
    #[must_use]
    pub fn new(pool: SqlitePool, config: BudgetConfig, pricing: LlmPricingConfig) -> Self {
        Self {
            pool,
            config: RwLock::new(config),
            pricing: RwLock::new(pricing),
            cached: Mutex::new(None),
            stop_logged: AtomicBool::new(false),
        }
    }

    /// Swap in the budget and the price list the operator just saved, and
    /// drop the memo so the next read answers against them.
    ///
    /// Both together because they are one question: a budget without the
    /// rates that price it against is a number with no scale.
    pub async fn replace_config(&self, config: BudgetConfig, pricing: LlmPricingConfig) {
        *self.config.write().await = config;
        *self.pricing.write().await = pricing;
        self.cached.lock().await.take();
    }

    /// Today's spend against today's budget, recomputed at most once every
    /// `SPEND_CACHE_TTL` and always after the UTC day turns over.
    ///
    /// # Errors
    ///
    /// Propagates a ledger read failure. A caller on the call path
    /// treats that as *not stopped*: a database hiccup must not take the
    /// deployment down, and the budget is a ceiling on spending rather
    /// than a safety interlock.
    pub async fn state(&self) -> crate::Result<BudgetState> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        // The memo stays locked across the recompute on purpose: it makes
        // the read single-flight, so a burst of calls arriving on a stale
        // memo costs one `GROUP BY` between them rather than one each.
        let mut slot = self.cached.lock().await;
        if let Some((at, state)) = slot.as_ref()
            && state.day == today
            && at.elapsed() < SPEND_CACHE_TTL
        {
            let memo = state.clone();
            drop(slot);
            return Ok(memo);
        }
        let fresh = self.compute(&today).await?;
        *slot = Some((Instant::now(), fresh.clone()));
        drop(slot);
        Ok(fresh)
    }

    /// Read the ledger and price today's rows. No caching here — that is
    /// [`Self::state`]'s job, and keeping the two apart is what lets the
    /// dashboard ask for an uncached figure when it wants one.
    async fn compute(&self, today: &str) -> crate::Result<BudgetState> {
        let config = self.config.read().await.clone();
        let pricing = self.pricing.read().await.clone();
        let buckets = usage::buckets(&self.pool, Some(today)).await?;
        let (spent, unpriced_calls) = usage::total_cost(&buckets, &pricing);
        let unlocked = crate::db::meta_get(&self.pool, UNLOCKED_DAY_KEY)
            .await
            .map_err(|e| crate::Error::Other(format!("budget unlocked day: {e}")))?
            .is_some_and(|d| d == today);
        Ok(BudgetState {
            day: today.to_owned(),
            spent,
            unpriced_calls,
            limit: config.daily_limit(),
            warn_fraction: config.warn_fraction(),
            unlocked,
            currency: pricing.currency,
        })
    }

    /// Lift the stop for the current UTC day.
    ///
    /// # Errors
    ///
    /// Propagates the `engine_meta` write failure.
    pub async fn unlock_today(&self) -> crate::Result<()> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        crate::db::meta_set(&self.pool, UNLOCKED_DAY_KEY, &today)
            .await
            .map_err(|e| crate::Error::Other(format!("budget unlock: {e}")))?;
        self.cached.lock().await.take();
        tracing::info!(day = %today, "budget: the stop was lifted for today");
        Ok(())
    }

    /// Put the budget back in force after an [`Self::unlock_today`].
    ///
    /// # Errors
    ///
    /// Propagates the `engine_meta` write failure.
    pub async fn relock_today(&self) -> crate::Result<()> {
        sqlx::query("DELETE FROM engine_meta WHERE key = ?")
            .bind(UNLOCKED_DAY_KEY)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::Error::Other(format!("budget relock: {e}")))?;
        self.cached.lock().await.take();
        tracing::info!("budget: the budget is back in force");
        Ok(())
    }

    /// The gate: emit whatever notice today has earned, then say whether
    /// this call may go ahead.
    ///
    /// `Some(message)` means refuse and hand the caller that sentence.
    /// Both jobs live in one function because they read the same state,
    /// and reading it twice is how the notice and the refusal would come
    /// to disagree about which day it is.
    pub async fn gate(&self) -> Option<String> {
        let state = match self.state().await {
            Ok(s) => s,
            Err(e) => {
                // A ledger we cannot read is not a budget we may enforce.
                tracing::warn!(error = %e, "budget: could not read today's spend — not enforcing");
                return None;
            },
        };
        self.notify(&state).await;
        if !state.stopped() {
            if self.stop_logged.swap(false, Ordering::Relaxed) {
                tracing::info!(day = %state.day, "budget: paid model calls resume");
            }
            return None;
        }
        let message = state.stop_message();
        if !self.stop_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                day = %state.day,
                spent = state.spent,
                limit = state.limit.unwrap_or(0.0),
                "budget: the daily budget is spent — paid model calls are stopped"
            );
        }
        Some(message)
    }

    /// Emit the operator notice for the highest threshold today crossed,
    /// at most once per threshold per day.
    async fn notify(&self, state: &BudgetState) {
        let Some(threshold) = state.crossed() else {
            return;
        };
        match self.claim_notice(&state.day, threshold).await {
            Ok(true) => {},
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(error = %e, "budget: could not claim the notice for today");
                return;
            },
        }
        let payload = serde_json::json!({
            "threshold": threshold.as_str(),
            // Crossing the line and being stopped by it are two facts: an
            // operator who unlocked the day still passed their budget, and
            // still wants to hear about it once, but nothing is refused.
            "stopped": state.stopped(),
            "day": state.day,
            "spent": state.spent,
            "limit": state.limit,
            "percent": state.percent_used(),
            "currency": state.currency,
            "unpriced_calls": state.unpriced_calls,
            "dashboard_path": "/dashboard/admin/usage",
        });
        if let Err(e) = crate::events::insert_event(
            &self.pool,
            crate::events::EventKind::BudgetThresholdReached,
            None,
            None,
            &payload,
        )
        .await
        {
            tracing::warn!(error = %e, "budget: could not emit the threshold notice");
        }
    }

    /// Claim the right to send `threshold`'s notice for `day`, returning
    /// whether this caller won it.
    ///
    /// One statement, so two concurrent calls crossing the line together
    /// cannot both send. `RETURNING` yields a row only when the upsert
    /// actually wrote, and the `WHERE` blocks the write when the day is
    /// already claimed.
    async fn claim_notice(&self, day: &str, threshold: Threshold) -> sqlx::Result<bool> {
        let key = format!("{NOTIFIED_DAY_PREFIX}{}", threshold.as_str());
        let claimed: Option<(String,)> = sqlx::query_as(
            "INSERT INTO engine_meta (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value \
               WHERE engine_meta.value <> excluded.value \
             RETURNING value",
        )
        .bind(&key)
        .bind(day)
        .fetch_optional(&self.pool)
        .await?;
        Ok(claimed.is_some())
    }
}

/// Decorator that refuses a metered call while the budget is reached.
///
/// It sits **outside** the retry wrapper and outside the ledger: a
/// refused call was never made, so it must not be retried and must not
/// appear in the ledger as a call the provider saw.
struct GatedBackend {
    inner: Box<dyn LlmBackend>,
    guard: Arc<BudgetGuard>,
}

#[async_trait]
impl LlmBackend for GatedBackend {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        match self.guard.gate().await {
            Some(message) => Err(LlmError::Budget(message)),
            None => self.inner.complete(request).await,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        match self.guard.gate().await {
            Some(message) => Err(LlmError::Budget(message)),
            None => self.inner.chat(request).await,
        }
    }

    async fn health_check(&self, probe: &CompletionRequest) -> Result<()> {
        // Delegated untouched: a probe is how an operator checks their
        // models still answer, and a stopped deployment that reported
        // every slot as broken would send them looking for the wrong
        // fault.
        self.inner.health_check(probe).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelPrice;
    use crate::llm::FakeLlmBackend;

    fn pricing(per_million: f64) -> LlmPricingConfig {
        LlmPricingConfig {
            currency: Some("EUR".to_owned()),
            models: vec![ModelPrice {
                model: "fake-model".to_owned(),
                input: per_million,
                cached_input: None,
                cache_write: None,
                output: per_million,
            }],
            extra: serde_yaml::Mapping::new(),
        }
    }

    fn capped(limit: f64, warn_at_percent: u8) -> BudgetConfig {
        BudgetConfig {
            daily_limit: Some(limit),
            warn_at_percent,
            extra: serde_yaml::Mapping::new(),
        }
    }

    async fn guard_over(
        config: BudgetConfig,
        pricing: LlmPricingConfig,
    ) -> (tempfile::TempDir, Arc<BudgetGuard>, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let guard = Arc::new(BudgetGuard::new(pool.clone(), config, pricing));
        (dir, guard, pool)
    }

    /// Seed `tokens` output tokens of metered spend into today's ledger.
    ///
    /// The column list is the one `usage::RecordingBackend::write` binds,
    /// because the coupling under test is exactly that: the guard prices
    /// the rows the ledger writes, through the same `usage::total_cost`.
    async fn spend(pool: &SqlitePool, tokens: i64) {
        sqlx::query(
            "INSERT INTO llm_usage
               (ts, slot, backend, model, kind, billing, source, tag,
                prompt_tokens, completion_tokens, cached_prompt_tokens,
                cache_write_tokens, latency_ms, error)
             VALUES (?, 'ingest', 'anthropic', 'fake-model', 'complete', 'api', 'serve', NULL,
                     0, ?, 0, 0, 10, NULL)",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(tokens)
        .execute(pool)
        .await
        .expect("seed usage");
    }

    /// The decision this feature exists to make, both ways round.
    #[tokio::test]
    async fn under_the_budget_a_metered_call_goes_through_and_over_it_it_does_not() {
        // 1.00 EUR per million output tokens, budget of 1.00 EUR.
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;

        // 500k tokens = 0.50 — half the budget.
        spend(&pool, 500_000).await;
        assert!(
            guard.gate().await.is_none(),
            "under the budget the call must be allowed"
        );

        // Another 600k takes the day to 1.10, past the budget.
        spend(&pool, 600_000).await;
        guard.cached.lock().await.take();
        let refusal = guard
            .gate()
            .await
            .expect("over the budget the call is refused");
        assert!(
            refusal.contains("daily budget"),
            "the refusal must say what stopped it: {refusal}"
        );
    }

    /// A refused call is refused, not degraded into a wrong answer, and
    /// it never reaches the provider.
    #[tokio::test]
    async fn the_gate_returns_a_budget_error_and_does_not_call_the_provider() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 2_000_000).await;

        let gated = maybe_gate_with(
            Box::new(FakeLlmBackend::new("fake-model", "should not be reached")),
            Billing::Api,
            Arc::clone(&guard),
        );
        let err = gated
            .complete(CompletionRequest::new("hello"))
            .await
            .expect_err("the budget must refuse");
        assert!(matches!(err, LlmError::Budget(_)), "got {err:?}");
    }

    /// A model on a flat subscription, or one running on this machine,
    /// costs nothing per token — so it is never gated, however far past
    /// the budget the day is.
    #[tokio::test]
    async fn a_call_that_costs_no_money_is_never_gated() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 5_000_000).await;
        assert!(guard.gate().await.is_some(), "the day really is stopped");

        for billing in [Billing::Subscription, Billing::Local] {
            let backend = maybe_gate_with(
                Box::new(FakeLlmBackend::new("fake-model", "answered")),
                billing,
                Arc::clone(&guard),
            );
            backend
                .complete(CompletionRequest::new("hello"))
                .await
                .unwrap_or_else(|e| panic!("{billing:?} must not be gated: {e}"));
        }
    }

    /// A probe is how the operator checks their models still answer. A
    /// stopped deployment that failed every probe would send them
    /// hunting a fault that is not there.
    #[tokio::test]
    async fn a_health_probe_still_reaches_the_provider_under_a_stop() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 5_000_000).await;
        let gated = maybe_gate_with(
            Box::new(FakeLlmBackend::new("fake-model", "pong")),
            Billing::Api,
            Arc::clone(&guard),
        );
        gated
            .health_check(&CompletionRequest::new("ping"))
            .await
            .expect("a probe is not spending");
    }

    /// No `budget:` section is no budget — not a budget of zero, which would
    /// stop a working deployment on an upgrade.
    #[tokio::test]
    async fn without_a_budget_nothing_is_ever_stopped() {
        let (_dir, guard, pool) = guard_over(BudgetConfig::default(), pricing(1.0)).await;
        spend(&pool, 50_000_000).await;
        let state = guard.state().await.expect("state");
        assert_eq!(state.limit, None);
        assert!(!state.stopped());
        assert_eq!(state.crossed(), None, "no budget, no threshold to cross");
        assert!(guard.gate().await.is_none());
    }

    /// The warning fires once a day, at the threshold, and does not
    /// stop anything.
    #[tokio::test]
    async fn the_warning_fires_once_a_day_and_stops_nothing() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        // 850k tokens = 0.85 — past 80% of the budget, short of it.
        spend(&pool, 850_000).await;

        assert!(guard.gate().await.is_none(), "a warning must not stop");
        assert!(guard.gate().await.is_none());
        guard.cached.lock().await.take();
        assert!(guard.gate().await.is_none());

        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM wiki_events ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("events");
        assert_eq!(
            kinds,
            vec!["budget_threshold_reached".to_owned()],
            "one notice for the day, however many calls cross the line"
        );
        let payload: String = sqlx::query_scalar("SELECT payload FROM wiki_events WHERE id = 1")
            .fetch_one(&pool)
            .await
            .expect("payload");
        assert!(payload.contains("\"threshold\":\"warn\""), "{payload}");
    }

    /// Warn and stop are one mechanism at two heights: crossing the budget
    /// after a warning sends a second, distinct notice.
    #[tokio::test]
    async fn crossing_the_budget_after_a_warning_sends_its_own_notice() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 850_000).await;
        assert!(guard.gate().await.is_none());

        spend(&pool, 300_000).await;
        guard.cached.lock().await.take();
        assert!(guard.gate().await.is_some(), "now it stops");

        let thresholds: Vec<String> = sqlx::query_scalar(
            "SELECT json_extract(payload, '$.threshold') FROM wiki_events ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("events");
        assert_eq!(thresholds, vec!["warn".to_owned(), "stop".to_owned()]);
    }

    /// Unlocking is what the dashboard button does, and it must let the
    /// day carry on without the operator having to edit YAML.
    #[tokio::test]
    async fn unlocking_the_day_resumes_spending_and_relocking_stops_it_again() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 2_000_000).await;
        assert!(guard.gate().await.is_some());

        guard.unlock_today().await.expect("unlock");
        assert!(guard.gate().await.is_none(), "an unlocked day spends again");
        let state = guard.state().await.expect("state");
        assert!(state.unlocked);
        assert!(
            !state.stopped() && state.spent > state.limit.expect("budget"),
            "an unlocked day still reports how far past the budget it went"
        );

        guard.relock_today().await.expect("relock");
        assert!(guard.gate().await.is_some(), "the budget is back in force");
    }

    /// The operator raising the budget from the dashboard must take effect
    /// on the next call, not on the next restart.
    #[tokio::test]
    async fn raising_the_budget_takes_effect_without_a_restart() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        spend(&pool, 2_000_000).await;
        assert!(guard.gate().await.is_some());

        guard.replace_config(capped(10.0, 80), pricing(1.0)).await;
        assert!(
            guard.gate().await.is_none(),
            "the new budget is in force now"
        );
    }

    /// The budget is read against the price list, so a model nobody priced
    /// spends nothing — and the state carries that count so the page can
    /// say the budget is looser than it looks.
    #[tokio::test]
    async fn an_unpriced_model_spends_nothing_and_is_counted_as_unpriced() {
        let (_dir, guard, pool) = guard_over(capped(1.0, 80), pricing(1.0)).await;
        sqlx::query(
            "INSERT INTO llm_usage
               (ts, slot, backend, model, kind, billing, source, tag,
                prompt_tokens, completion_tokens, cached_prompt_tokens,
                cache_write_tokens, latency_ms, error)
             VALUES (?, 'ingest', 'anthropic', 'unpriced-model', 'complete', 'api', 'serve',
                     NULL, 0, 9000000, 0, 0, 10, NULL)",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .expect("seed");

        let state = guard.state().await.expect("state");
        assert!((state.spent - 0.0).abs() < f64::EPSILON);
        assert_eq!(state.unpriced_calls, 1);
        assert!(!state.stopped());
    }

    /// The figure the operator reads beside the money it came from.
    ///
    /// 4.60 of 4.00 is 115%. Flooring the ratio prints 114%, because
    /// 4.6 / 4.0 is 1.1499999999999999 in binary floating point — a
    /// whole point lost on the one number this block exists to show.
    #[test]
    fn the_percentage_matches_the_two_figures_it_is_computed_from() {
        let state = BudgetState {
            day: "2026-09-06".to_owned(),
            spent: 4.6,
            unpriced_calls: 0,
            limit: Some(4.0),
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        };
        assert_eq!(state.percent_used(), Some(115));
    }

    /// The stop is one wording, and it is read as a sentence on the
    /// dashboard as well as after a colon in a log line.
    #[test]
    fn the_stop_message_reads_as_a_sentence() {
        let state = BudgetState {
            day: "2026-09-06".to_owned(),
            spent: 4.6,
            unpriced_calls: 0,
            limit: Some(4.0),
            warn_fraction: 0.8,
            unlocked: false,
            currency: Some("EUR".to_owned()),
        };
        let msg = state.stop_message();
        assert!(msg.starts_with("The daily budget"), "{msg}");
        assert!(msg.ends_with('.'), "{msg}");
        assert!(msg.contains("4.60 EUR of 4.00 EUR"), "{msg}");
    }

    /// A hand-edited `warn_at_percent: 250` must not push the warning
    /// past the stop it is supposed to precede.
    #[test]
    fn an_out_of_range_warn_threshold_is_clamped_to_the_budget() {
        assert!((capped(1.0, 250).warn_fraction() - 1.0).abs() < f64::EPSILON);
        assert!((capped(1.0, 80).warn_fraction() - 0.8).abs() < f64::EPSILON);
    }

    /// A zero or negative budget is a typo, and reading it as "stop
    /// everything" would answer nothing all day.
    #[test]
    fn a_zero_budget_is_no_budget() {
        assert_eq!(capped(0.0, 80).daily_limit(), None);
        assert_eq!(capped(-5.0, 80).daily_limit(), None);
        assert_eq!(capped(5.0, 80).daily_limit(), Some(5.0));
    }

    /// Test-only variant of [`maybe_gate`] that takes the guard
    /// explicitly: the real one reads the process-wide `OnceLock`, which
    /// the tests in this binary share.
    fn maybe_gate_with(
        inner: Box<dyn LlmBackend>,
        billing: Billing,
        guard: Arc<BudgetGuard>,
    ) -> Box<dyn LlmBackend> {
        if billing.is_metered() {
            Box::new(GatedBackend { inner, guard })
        } else {
            inner
        }
    }
}
