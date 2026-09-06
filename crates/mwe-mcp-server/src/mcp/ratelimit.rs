// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-token call ceilings for the MCP tool surface.
//!
//! The dispatcher is the one place every tool call passes through, so the
//! ceilings are counted there, once, against the token that made the call
//! — see [`super::dispatch`]. What a ceiling protects against is not an
//! attacker with a password: it is a token that has left its owner's
//! hands, and a consumer whose loop lost its brakes. Both look identical
//! from here, and both are stopped by the same counter.
//!
//! ## Shape
//!
//! Fixed windows, in memory, one bucket per `(token, rate_limit_id)`.
//! Fixed rather than sliding because the answer only has to be right at
//! the scale of the window: a caller who spends a minute's allowance in
//! the first second waits for the next minute, which is the behaviour
//! wanted. In memory rather than in the database because a ceiling is
//! about the process that is serving right now, and a counter that
//! survives a restart would buy nothing a restart does not already
//! reset.
//!
//! Per token rather than per profile: a profile shared by five consumers
//! would let one runaway starve the other four, and per token the person
//! whose token it is is the person who feels it.
//!
//! ## Two pairs of ceilings
//!
//! Every call counts against the call ceilings. A call that puts a model
//! or the embedder to work — the list is
//! [`super::MODEL_COST_TOOLS`] — counts against the model ceilings as
//! well, which are lower: those are the calls that arrive on somebody's
//! invoice.
//!
//! An over-ceiling call still counts. A caller who is being refused and
//! keeps calling is exactly the caller the ceiling is for, and a refusal
//! that reset the accounting would hand them an unmetered retry loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mwe_core::config::{RateLimitProfile, RateLimitsConfig};
use parking_lot::Mutex;

/// Length of the short window.
const MINUTE: Duration = Duration::from_secs(60);
/// Length of the long window.
const HOUR: Duration = Duration::from_secs(60 * 60);

/// Idle buckets are dropped after this long, so the map cannot grow with
/// the number of tokens a deployment has ever seen.
const PRUNE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// Which ceiling a refusal hit, for the message the caller reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceiling {
    /// Calls of any kind, per minute.
    CallsPerMinute,
    /// Calls of any kind, per hour.
    CallsPerHour,
    /// Calls that spend a model or the embedder, per minute.
    ModelCallsPerMinute,
    /// Calls that spend a model or the embedder, per hour.
    ModelCallsPerHour,
}

impl Ceiling {
    /// The config key the operator raises to lift this ceiling.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::CallsPerMinute => "calls_per_minute",
            Self::CallsPerHour => "calls_per_hour",
            Self::ModelCallsPerMinute => "model_calls_per_minute",
            Self::ModelCallsPerHour => "model_calls_per_hour",
        }
    }
}

/// A refused call: which ceiling stopped it, at what value, and how long
/// until the window that holds it rolls over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// The ceiling that was exceeded.
    pub ceiling: Ceiling,
    /// Its configured value.
    pub limit: u32,
    /// Seconds until the offending window resets. Always ≥ 1, so a
    /// caller that honours it never retries into the same window.
    pub retry_after_secs: u64,
}

/// One fixed window: when it started and how many calls landed in it.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

impl Window {
    const fn new(now: Instant) -> Self {
        Self {
            start: now,
            count: 0,
        }
    }

    /// Count one call, rolling the window over first when it has aged
    /// out. Returns the new count.
    fn hit(&mut self, now: Instant, len: Duration) -> u32 {
        if now.duration_since(self.start) >= len {
            self.start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count
    }

    /// Seconds until this window rolls over, at least one.
    fn retry_after(&self, now: Instant, len: Duration) -> u64 {
        let elapsed = now.duration_since(self.start);
        let left = len.saturating_sub(elapsed);
        left.as_secs().max(1)
    }
}

/// The four windows one bucket keeps.
#[derive(Debug, Clone, Copy)]
struct Buckets {
    calls_minute: Window,
    calls_hour: Window,
    model_minute: Window,
    model_hour: Window,
    last_seen: Instant,
}

impl Buckets {
    const fn new(now: Instant) -> Self {
        Self {
            calls_minute: Window::new(now),
            calls_hour: Window::new(now),
            model_minute: Window::new(now),
            model_hour: Window::new(now),
            last_seen: now,
        }
    }
}

/// The live counters, plus the ceilings they are read against.
///
/// One instance per running server, shared through
/// [`McpState`](super::state::McpState).
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitsConfig,
    buckets: Mutex<HashMap<String, Buckets>>,
}

impl RateLimiter {
    /// Build a limiter enforcing `config`.
    #[must_use]
    pub fn new(config: RateLimitsConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The ceilings a token carrying `rate_limit_id` is held to.
    #[must_use]
    pub fn profile(&self, rate_limit_id: &str) -> RateLimitProfile {
        self.config.profile(rate_limit_id)
    }

    /// Count one call and say whether it may proceed.
    ///
    /// `token` identifies the bucket — the JWT id, so one token's traffic
    /// is one token's business. `spends_model` puts the call against the
    /// model ceilings as well as the call ceilings.
    ///
    /// # Errors
    ///
    /// [`Refusal`] naming the ceiling that stopped the call and when the
    /// caller may try again.
    pub fn check(
        &self,
        token: &str,
        rate_limit_id: &str,
        spends_model: bool,
    ) -> Result<(), Refusal> {
        self.check_at(Instant::now(), token, rate_limit_id, spends_model)
    }

    /// [`Self::check`] against an injected clock (tests).
    fn check_at(
        &self,
        now: Instant,
        token: &str,
        rate_limit_id: &str,
        spends_model: bool,
    ) -> Result<(), Refusal> {
        let limits = self.config.profile(rate_limit_id);
        let key = format!("{rate_limit_id}\u{1f}{token}");

        let mut guard = self.buckets.lock();
        guard.retain(|_, b| now.duration_since(b.last_seen) < PRUNE_AFTER);
        let bucket = guard.entry(key).or_insert_with(|| Buckets::new(now));
        bucket.last_seen = now;

        // Every applicable window is advanced before any of them is
        // judged: a short-circuit would leave one axis unaccounted for
        // whenever another refused first.
        let calls_minute = bucket.calls_minute.hit(now, MINUTE);
        let calls_hour = bucket.calls_hour.hit(now, HOUR);
        let (model_minute, model_hour) = if spends_model {
            (
                bucket.model_minute.hit(now, MINUTE),
                bucket.model_hour.hit(now, HOUR),
            )
        } else {
            (0, 0)
        };

        // Judged narrowest-first, so the message names the ceiling the
        // caller will actually wait on.
        let over = [
            (
                spends_model && model_minute > limits.model_calls_per_minute,
                Ceiling::ModelCallsPerMinute,
                limits.model_calls_per_minute,
                bucket.model_minute.retry_after(now, MINUTE),
            ),
            (
                calls_minute > limits.calls_per_minute,
                Ceiling::CallsPerMinute,
                limits.calls_per_minute,
                bucket.calls_minute.retry_after(now, MINUTE),
            ),
            (
                spends_model && model_hour > limits.model_calls_per_hour,
                Ceiling::ModelCallsPerHour,
                limits.model_calls_per_hour,
                bucket.model_hour.retry_after(now, HOUR),
            ),
            (
                calls_hour > limits.calls_per_hour,
                Ceiling::CallsPerHour,
                limits.calls_per_hour,
                bucket.calls_hour.retry_after(now, HOUR),
            ),
        ]
        .into_iter()
        .find_map(|(hit, ceiling, limit, retry_after_secs)| {
            hit.then_some(Refusal {
                ceiling,
                limit,
                retry_after_secs,
            })
        });
        drop(guard);

        over.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mwe_core::config::DASHBOARD_RATE_LIMIT_ID;

    fn limiter() -> RateLimiter {
        RateLimiter::new(RateLimitsConfig::default())
    }

    /// The built-in call ceiling binds a token that names no profile at
    /// all: call 120 passes and call 121 does not.
    #[test]
    fn the_default_ceiling_is_enforced_without_a_config_section() {
        let rl = limiter();
        let now = Instant::now();
        for i in 1..=120 {
            assert!(
                rl.check_at(now, "tok", "default", false).is_ok(),
                "call {i} is inside the 120-a-minute ceiling"
            );
        }
        let refusal = rl
            .check_at(now, "tok", "default", false)
            .expect_err("call 121 is over the ceiling");
        assert_eq!(refusal.ceiling, Ceiling::CallsPerMinute);
        assert_eq!(refusal.limit, 120);
        assert!(refusal.retry_after_secs >= 1);
    }

    /// The model ceiling is the lower of the two and bites first: 30
    /// model calls pass, the 31st is refused while a plain call at the
    /// same moment still goes through.
    #[test]
    fn a_model_call_hits_the_model_ceiling_first() {
        let rl = limiter();
        let now = Instant::now();
        for _ in 0..30 {
            rl.check_at(now, "tok", "default", true).expect("inside");
        }
        let refusal = rl
            .check_at(now, "tok", "default", true)
            .expect_err("the 31st model call is over the model ceiling");
        assert_eq!(refusal.ceiling, Ceiling::ModelCallsPerMinute);
        assert_eq!(refusal.limit, 30);
        assert!(
            rl.check_at(now, "tok", "default", false).is_ok(),
            "a call that spends no model is still inside the call ceiling"
        );
    }

    /// The window rolls over: the same token is served again a minute
    /// later, at the same ceiling.
    #[test]
    fn the_minute_window_rolls_over() {
        let rl = limiter();
        let now = Instant::now();
        for _ in 0..121 {
            let _ = rl.check_at(now, "tok", "default", false);
        }
        let later = now + Duration::from_secs(61);
        assert!(rl.check_at(later, "tok", "default", false).is_ok());
    }

    /// Two tokens on the same profile do not share a bucket — one
    /// consumer in a loop must not lock the others out.
    #[test]
    fn two_tokens_do_not_share_a_bucket() {
        let rl = limiter();
        let now = Instant::now();
        for _ in 0..121 {
            let _ = rl.check_at(now, "loud", "default", false);
        }
        assert!(rl.check_at(now, "quiet", "default", false).is_ok());
    }

    /// The `dashboard` profile is wider than the default one, and it is
    /// wider **without** a config section — the built-in for that name is
    /// its own, not the default profile's.
    #[test]
    fn the_dashboard_profile_is_wider_than_the_default() {
        let rl = limiter();
        let now = Instant::now();
        for i in 1..=200 {
            assert!(
                rl.check_at(now, "panel", DASHBOARD_RATE_LIMIT_ID, false)
                    .is_ok(),
                "dashboard call {i} is served"
            );
        }
        let refused_on_default =
            (1..=200).any(|_| rl.check_at(now, "plain", "default", false).is_err());
        assert!(
            refused_on_default,
            "the same 200 calls on the default profile are refused — the two \
             profiles are not the same ceiling"
        );
    }

    /// A `rate_limit_id` nobody configured falls back to the `default`
    /// profile's ceilings — a token cannot name its way out of one.
    #[test]
    fn an_unknown_profile_falls_back_to_default_not_to_unlimited() {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "default".to_owned(),
            RateLimitProfile {
                calls_per_minute: 2,
                ..RateLimitProfile::default()
            },
        );
        let rl = RateLimiter::new(RateLimitsConfig { profiles });
        let now = Instant::now();
        assert!(rl.check_at(now, "tok", "invented", false).is_ok());
        assert!(rl.check_at(now, "tok", "invented", false).is_ok());
        let refusal = rl
            .check_at(now, "tok", "invented", false)
            .expect_err("the third call is over the operator's own ceiling of 2");
        assert_eq!(refusal.limit, 2);
    }

    /// An operator's profile replaces the built-in numbers for the name
    /// it declares.
    #[test]
    fn a_configured_profile_replaces_the_builtin_numbers() {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "nightly-import".to_owned(),
            RateLimitProfile {
                model_calls_per_minute: 1,
                ..RateLimitProfile::default()
            },
        );
        let rl = RateLimiter::new(RateLimitsConfig { profiles });
        let now = Instant::now();
        assert!(rl.check_at(now, "tok", "nightly-import", true).is_ok());
        let refusal = rl
            .check_at(now, "tok", "nightly-import", true)
            .expect_err("the second model call is over the declared ceiling of 1");
        assert_eq!(refusal.ceiling, Ceiling::ModelCallsPerMinute);
        assert_eq!(refusal.limit, 1);
    }
}
