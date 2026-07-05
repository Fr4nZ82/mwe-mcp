// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tiny process-global fixed-window rate limiter (roadmap 28).
//!
//! Shared by the password-recovery request route and the 2FA challenge.
//! A single-process server, so an in-memory map is enough; entries are
//! pruned opportunistically so it cannot grow without bound.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

struct Window {
    start: Instant,
    count: u32,
}

static LIMITER: LazyLock<Mutex<HashMap<String, Window>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Prune horizon for idle entries.
///
/// Entries older than this are dropped on the next check — decoupled from
/// any single caller's window so mixed windows don't evict each other early.
const PRUNE_AFTER: Duration = Duration::from_secs(3600);

/// Record one hit against `key`; return whether it is within the limit.
///
/// At most `max` hits per `window`. Always advances the counter, so a
/// caller rate-limiting on several axes must evaluate each (no short-circuit).
#[must_use]
pub fn check(key: &str, max: u32, window: Duration) -> bool {
    let now = Instant::now();
    let mut g = LIMITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.retain(|_, w| now.duration_since(w.start) <= PRUNE_AFTER);
    let w = g.entry(key.to_owned()).or_insert(Window {
        start: now,
        count: 0,
    });
    if now.duration_since(w.start) > window {
        w.start = now;
        w.count = 0;
    }
    w.count += 1;
    let allowed = w.count <= max;
    drop(g);
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let key = "test:allows_up_to_max";
        let win = Duration::from_secs(60);
        assert!(check(key, 3, win));
        assert!(check(key, 3, win));
        assert!(check(key, 3, win));
        assert!(!check(key, 3, win), "the 4th hit is over the limit");
    }

    #[test]
    fn separate_keys_are_independent() {
        let win = Duration::from_secs(60);
        assert!(check("test:key-a-indep", 1, win));
        assert!(check("test:key-b-indep", 1, win));
    }
}
