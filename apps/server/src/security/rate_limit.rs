//! In-memory token-bucket rate limiter.
//!
//! Redis-backed multi-node limiting is deferred; this is the single-node path.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Configuration for a token-bucket limiter.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Maximum tokens (burst size).
    pub capacity: f64,
    /// Tokens added per second.
    pub refill_per_sec: f64,
}

impl RateLimitConfig {
    /// Build a limit of `max_events` evenly over `window`.
    pub fn per_window(max_events: u32, window: Duration) -> Self {
        let secs = window.as_secs_f64().max(0.001);
        let capacity = f64::from(max_events);
        Self {
            capacity,
            refill_per_sec: capacity / secs,
        }
    }
}

/// Default register limit: 30 registrations / minute / key (IP).
pub fn default_register_config() -> RateLimitConfig {
    RateLimitConfig::per_window(30, Duration::from_secs(60))
}

/// Default refresh limit: 60 attempts / minute / key.
pub fn default_refresh_config() -> RateLimitConfig {
    RateLimitConfig::per_window(60, Duration::from_secs(60))
}

/// Default session_intent limit: 20 intents / minute / key.
pub fn default_session_intent_config() -> RateLimitConfig {
    RateLimitConfig::per_window(20, Duration::from_secs(60))
}

/// Error when a key has exhausted its budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitExceeded {
    /// Suggested client wait before retry.
    pub retry_after: Duration,
}

impl std::fmt::Display for RateLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rate limit exceeded; retry after {}s",
            self.retry_after.as_secs().max(1)
        )
    }
}

impl std::error::Error for RateLimitExceeded {}

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Thread-safe per-key token-bucket rate limiter.
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Try to consume one token for `key` at `now`.
    pub fn check(&self, key: &str, now: Instant) -> Result<(), RateLimitExceeded> {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = map.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: self.config.capacity,
            last_refill: now,
        });

        refill(bucket, &self.config, now);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let need = 1.0 - bucket.tokens;
            let secs = if self.config.refill_per_sec > 0.0 {
                need / self.config.refill_per_sec
            } else {
                60.0
            };
            Err(RateLimitExceeded {
                retry_after: Duration::from_secs_f64(secs.max(0.001)),
            })
        }
    }

    /// Convenience: check with wall clock.
    pub fn check_now(&self, key: &str) -> Result<(), RateLimitExceeded> {
        self.check(key, Instant::now())
    }

    /// Current token estimate for tests / metrics (does not consume).
    pub fn tokens(&self, key: &str, now: Instant) -> f64 {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = map.get_mut(key) else {
            return self.config.capacity;
        };
        refill(bucket, &self.config, now);
        bucket.tokens
    }

    /// Drop all buckets (tests).
    pub fn clear(&self) {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }
}

fn refill(bucket: &mut Bucket, config: &RateLimitConfig, now: Instant) {
    let elapsed = now
        .saturating_duration_since(bucket.last_refill)
        .as_secs_f64();
    if elapsed > 0.0 {
        bucket.tokens = (bucket.tokens + elapsed * config.refill_per_sec).min(config.capacity);
        bucket.last_refill = now;
    }
}

/// Bundle of named limiters used by the server.
#[derive(Debug)]
pub struct RateLimiters {
    pub register: RateLimiter,
    pub refresh: RateLimiter,
    pub session_intent: RateLimiter,
}

impl RateLimiters {
    pub fn new() -> Self {
        Self {
            register: RateLimiter::new(default_register_config()),
            refresh: RateLimiter::new(default_refresh_config()),
            session_intent: RateLimiter::new(default_session_intent_config()),
        }
    }

    /// Construct with custom configs (tests).
    pub fn with_configs(
        register: RateLimitConfig,
        refresh: RateLimitConfig,
        session_intent: RateLimitConfig,
    ) -> Self {
        Self {
            register: RateLimiter::new(register),
            refresh: RateLimiter::new(refresh),
            session_intent: RateLimiter::new(session_intent),
        }
    }
}

impl Default for RateLimiters {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_rejects() {
        let lim = RateLimiter::new(RateLimitConfig {
            capacity: 3.0,
            refill_per_sec: 0.0, // no refill
        });
        let t0 = Instant::now();
        assert!(lim.check("k", t0).is_ok());
        assert!(lim.check("k", t0).is_ok());
        assert!(lim.check("k", t0).is_ok());
        let err = lim.check("k", t0).unwrap_err();
        assert!(err.retry_after > Duration::ZERO);
    }

    #[test]
    fn refills_over_time() {
        let lim = RateLimiter::new(RateLimitConfig {
            capacity: 2.0,
            refill_per_sec: 10.0, // full refill in 0.2s
        });
        let t0 = Instant::now();
        assert!(lim.check("k", t0).is_ok());
        assert!(lim.check("k", t0).is_ok());
        assert!(lim.check("k", t0).is_err());

        let t1 = t0 + Duration::from_millis(150);
        // ~1.5 tokens refilled
        assert!(lim.check("k", t1).is_ok());
    }

    #[test]
    fn keys_are_independent() {
        let lim = RateLimiter::new(RateLimitConfig {
            capacity: 1.0,
            refill_per_sec: 0.0,
        });
        let t0 = Instant::now();
        assert!(lim.check("a", t0).is_ok());
        assert!(lim.check("a", t0).is_err());
        assert!(lim.check("b", t0).is_ok());
    }

    #[test]
    fn per_window_config() {
        let cfg = RateLimitConfig::per_window(60, Duration::from_secs(60));
        assert!((cfg.capacity - 60.0).abs() < f64::EPSILON);
        assert!((cfg.refill_per_sec - 1.0).abs() < 1e-9);
    }
}
