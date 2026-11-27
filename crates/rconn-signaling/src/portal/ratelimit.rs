//! In-memory sliding-window rate limits (the server is single-node).

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: usize,
    pub per: Duration,
}

impl Limit {
    pub const fn new(max: usize, per: Duration) -> Self {
        Self { max, per }
    }
}

/// Keys kept before idle ones are swept.
const SWEEP_AT: usize = 20_000;
/// Longest window any limit uses; older hits are forgotten in a sweep.
const LONGEST_WINDOW: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub struct RateLimiter {
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    /// Count a hit on `key`; false (and not counted) when `limit` is already reached.
    pub fn hit(&self, key: &str, limit: Limit) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().expect("rate limit lock poisoned");
        if hits.len() >= SWEEP_AT {
            hits.retain(|_, q| q.back().is_some_and(|t| now - *t < LONGEST_WINDOW));
        }
        let q = hits.entry(key.to_owned()).or_default();
        while q.front().is_some_and(|t| now - *t >= limit.per) {
            q.pop_front();
        }
        if q.len() >= limit.max {
            return false;
        }
        q.push_back(now);
        true
    }
}

/// The rate-limit key for a client address: IPv4 as is, IPv6 by /64.
pub fn ip_key(ip: Option<IpAddr>) -> String {
    match ip {
        Some(ip) => crate::public::source_bucket(ip).to_string(),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_caps_and_keys_are_separate() {
        let rl = RateLimiter::default();
        let l = Limit::new(2, Duration::from_secs(60));
        assert!(rl.hit("a", l));
        assert!(rl.hit("a", l));
        assert!(!rl.hit("a", l));
        assert!(rl.hit("b", l));
        let short = Limit::new(1, Duration::ZERO);
        assert!(rl.hit("c", short));
        assert!(rl.hit("c", short), "an expired window forgets old hits");
    }
}
