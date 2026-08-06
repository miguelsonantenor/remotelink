//! Auth failure tracking with exponential backoff lockout.
//!
//! Keys are typically `ip:{addr}`, `device:{public_id}`, or
//! `device_ip:{public_id}:{addr}` (DESIGN.md Redis `auth_fail:*` shape).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Lockout policy for consecutive auth failures.
#[derive(Debug, Clone, Copy)]
pub struct AuthAttemptConfig {
    /// Failures allowed before the first lockout engages.
    pub max_failures_before_lockout: u32,
    /// Base lockout duration after the first threshold breach.
    pub base_lockout: Duration,
    /// Cap on exponential lockout growth.
    pub max_lockout: Duration,
    /// Quiet period after which the failure counter resets.
    pub failure_window: Duration,
}

impl Default for AuthAttemptConfig {
    fn default() -> Self {
        Self {
            max_failures_before_lockout: 5,
            base_lockout: Duration::from_secs(2),
            max_lockout: Duration::from_secs(3600),
            failure_window: Duration::from_secs(15 * 60),
        }
    }
}

/// Active lockout for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockoutActive {
    pub retry_after: Duration,
    pub failures: u32,
}

impl std::fmt::Display for LockoutActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many auth attempts; retry after {}s",
            self.retry_after.as_secs().max(1)
        )
    }
}

impl std::error::Error for LockoutActive {}

#[derive(Debug, Clone)]
struct AttemptRecord {
    failures: u32,
    last_failure: Instant,
    locked_until: Option<Instant>,
}

/// In-memory consecutive-failure tracker with exponential backoff.
#[derive(Debug)]
pub struct AuthAttemptTracker {
    config: AuthAttemptConfig,
    records: Mutex<HashMap<String, AttemptRecord>>,
}

impl AuthAttemptTracker {
    pub fn new(config: AuthAttemptConfig) -> Self {
        Self {
            config,
            records: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(AuthAttemptConfig::default())
    }

    /// Key helpers matching DESIGN.md naming.
    pub fn key_ip(ip: &str) -> String {
        format!("ip:{ip}")
    }

    pub fn key_device(device_public_id: &str) -> String {
        format!("device:{device_public_id}")
    }

    pub fn key_device_ip(device_public_id: &str, ip: &str) -> String {
        format!("device_ip:{device_public_id}:{ip}")
    }

    /// Return `Err` if the key is currently locked out.
    pub fn check(&self, key: &str, now: Instant) -> Result<(), LockoutActive> {
        let mut map = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rec) = map.get_mut(key) else {
            return Ok(());
        };

        // Reset stale failure windows when not locked.
        if rec.locked_until.is_none()
            && now.saturating_duration_since(rec.last_failure) > self.config.failure_window
        {
            map.remove(key);
            return Ok(());
        }

        if let Some(until) = rec.locked_until {
            if now < until {
                return Err(LockoutActive {
                    retry_after: until.saturating_duration_since(now),
                    failures: rec.failures,
                });
            }
            // Lockout expired; keep failure count for next exponential step.
            rec.locked_until = None;
        }
        Ok(())
    }

    pub fn check_now(&self, key: &str) -> Result<(), LockoutActive> {
        self.check(key, Instant::now())
    }

    /// Record a failed auth attempt; may engage or extend lockout.
    ///
    /// Returns the new lockout duration if one is active after this failure.
    pub fn record_failure(&self, key: &str, now: Instant) -> Option<Duration> {
        let mut map = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let rec = map.entry(key.to_string()).or_insert(AttemptRecord {
            failures: 0,
            last_failure: now,
            locked_until: None,
        });

        // Quiet period reset.
        if now.saturating_duration_since(rec.last_failure) > self.config.failure_window {
            rec.failures = 0;
            rec.locked_until = None;
        }

        rec.failures = rec.failures.saturating_add(1);
        rec.last_failure = now;

        if rec.failures >= self.config.max_failures_before_lockout {
            let over = rec
                .failures
                .saturating_sub(self.config.max_failures_before_lockout);
            let lockout = compute_lockout(self.config.base_lockout, self.config.max_lockout, over);
            rec.locked_until = Some(now + lockout);
            Some(lockout)
        } else if let Some(until) = rec.locked_until {
            if now < until {
                Some(until.saturating_duration_since(now))
            } else {
                rec.locked_until = None;
                None
            }
        } else {
            None
        }
    }

    pub fn record_failure_now(&self, key: &str) -> Option<Duration> {
        self.record_failure(key, Instant::now())
    }

    /// Clear the counter after a successful auth.
    pub fn record_success(&self, key: &str) {
        let mut map = self.records.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(key);
    }

    /// Failure count for tests / metrics.
    pub fn failures(&self, key: &str) -> u32 {
        let map = self.records.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key).map(|r| r.failures).unwrap_or(0)
    }

    pub fn clear(&self) {
        let mut map = self.records.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }
}

impl Default for AuthAttemptTracker {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// `base * 2^over_threshold`, capped at `max`.
fn compute_lockout(base: Duration, max: Duration, over_threshold: u32) -> Duration {
    let mult = 1u64 << over_threshold.min(20);
    let secs = base.as_secs().saturating_mul(mult);
    Duration::from_secs(secs.min(max.as_secs()).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tight_config() -> AuthAttemptConfig {
        AuthAttemptConfig {
            max_failures_before_lockout: 3,
            base_lockout: Duration::from_secs(4),
            max_lockout: Duration::from_secs(64),
            failure_window: Duration::from_secs(300),
        }
    }

    #[test]
    fn no_lockout_before_threshold() {
        let t = AuthAttemptTracker::new(tight_config());
        let now = Instant::now();
        assert!(t.check("k", now).is_ok());
        assert!(t.record_failure("k", now).is_none());
        assert!(t.record_failure("k", now).is_none());
        assert_eq!(t.failures("k"), 2);
        assert!(t.check("k", now).is_ok());
    }

    #[test]
    fn locks_after_threshold_with_exponential_backoff() {
        let t = AuthAttemptTracker::new(tight_config());
        let now = Instant::now();
        t.record_failure("k", now);
        t.record_failure("k", now);
        let lock1 = t.record_failure("k", now).expect("lockout");
        assert_eq!(lock1, Duration::from_secs(4)); // 2^0 * base
        assert!(t.check("k", now).is_err());

        // After lockout expires, next failure doubles.
        let later = now + Duration::from_secs(5);
        assert!(t.check("k", later).is_ok());
        let lock2 = t.record_failure("k", later).expect("lockout");
        assert_eq!(lock2, Duration::from_secs(8)); // 2^1 * base
    }

    #[test]
    fn success_clears_failures() {
        let t = AuthAttemptTracker::new(tight_config());
        let now = Instant::now();
        t.record_failure("k", now);
        t.record_failure("k", now);
        t.record_success("k");
        assert_eq!(t.failures("k"), 0);
        assert!(t.check("k", now).is_ok());
    }

    #[test]
    fn max_lockout_caps_growth() {
        let cfg = AuthAttemptConfig {
            max_failures_before_lockout: 1,
            base_lockout: Duration::from_secs(10),
            max_lockout: Duration::from_secs(30),
            failure_window: Duration::from_secs(600),
        };
        let t = AuthAttemptTracker::new(cfg);
        let now = Instant::now();
        // over=0 → 10s
        assert_eq!(t.record_failure("k", now), Some(Duration::from_secs(10)));
        let t1 = now + Duration::from_secs(11);
        // over=1 → 20s
        assert_eq!(t.record_failure("k", t1), Some(Duration::from_secs(20)));
        let t2 = t1 + Duration::from_secs(21);
        // over=2 → 40s capped to 30s
        assert_eq!(t.record_failure("k", t2), Some(Duration::from_secs(30)));
    }

    #[test]
    fn key_helpers() {
        assert_eq!(AuthAttemptTracker::key_ip("1.2.3.4"), "ip:1.2.3.4");
        assert_eq!(
            AuthAttemptTracker::key_device("1234567897"),
            "device:1234567897"
        );
        assert_eq!(
            AuthAttemptTracker::key_device_ip("1234567897", "1.2.3.4"),
            "device_ip:1234567897:1.2.3.4"
        );
    }
}
