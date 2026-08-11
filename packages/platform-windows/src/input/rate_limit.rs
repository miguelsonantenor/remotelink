//! Host-side input rate limiter (DESIGN: max 200 events/s).

use std::time::Instant;

use remotelink_protocol::InputEvent;

use super::{InjectError, InputInjector};

/// Hard cap on injected events per second (DESIGN host limits).
pub const MAX_INPUT_EVENTS_PER_SEC: u32 = 200;

/// Cumulative counters for inject path observability.
///
/// `dropped_rate_limit` is the DESIGN metric `input_drop_rate` (event count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputMetrics {
    /// Events successfully passed to the inner injector.
    pub accepted: u64,
    /// Events dropped because the per-second cap was exceeded.
    pub dropped_rate_limit: u64,
}

/// Fixed 1-second window rate limiter wrapping an [`InputInjector`].
///
/// When more than `max_per_sec` events arrive within the current window, further
/// events return `Ok(false)` without calling the inner injector, and
/// [`InputMetrics::dropped_rate_limit`] is incremented.
///
/// The window resets when `elapsed >= 1s` from `window_start` (not a sliding
/// token bucket). A burst at a window boundary can briefly admit up to
/// ~2× `max_per_sec` across the boundary; long-term average still respects the
/// DESIGN 200 events/s host cap.
pub struct RateLimitedInjector<I> {
    inner: I,
    max_per_sec: u32,
    window_start: Instant,
    window_count: u32,
    metrics: InputMetrics,
}

impl<I> RateLimitedInjector<I> {
    /// Wrap `inner` with a `max_per_sec` cap (clamped to at least 1).
    pub fn new(inner: I, max_per_sec: u32) -> Self {
        Self {
            inner,
            max_per_sec: max_per_sec.max(1),
            window_start: Instant::now(),
            window_count: 0,
            metrics: InputMetrics::default(),
        }
    }

    /// Current metrics snapshot.
    pub fn metrics(&self) -> InputMetrics {
        self.metrics
    }

    /// Borrow the inner injector.
    pub fn inner(&self) -> &I {
        &self.inner
    }

    /// Mutably borrow the inner injector.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    /// Configured max events per second.
    pub fn max_per_sec(&self) -> u32 {
        self.max_per_sec
    }

    fn roll_window_if_needed(&mut self) {
        if self.window_start.elapsed().as_secs() >= 1 {
            self.window_start = Instant::now();
            self.window_count = 0;
        }
    }
}

impl<I: InputInjector> RateLimitedInjector<I> {
    /// Try to inject `event`.
    ///
    /// Returns `Ok(true)` if injected, `Ok(false)` if dropped by the rate limit,
    /// or `Err` if the inner injector failed.
    pub fn try_inject(&mut self, event: &InputEvent) -> Result<bool, InjectError> {
        self.roll_window_if_needed();
        if self.window_count >= self.max_per_sec {
            self.metrics.dropped_rate_limit = self.metrics.dropped_rate_limit.saturating_add(1);
            return Ok(false);
        }
        self.inner.inject(event)?;
        self.window_count = self.window_count.saturating_add(1);
        self.metrics.accepted = self.metrics.accepted.saturating_add(1);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::StubInjector;
    use remotelink_protocol::{InputPayload, MouseMove};

    fn mv(seq: u32) -> InputEvent {
        InputEvent {
            client_ts_us: u64::from(seq),
            seq,
            payload: InputPayload::MouseMove(MouseMove {
                x: 0.0,
                y: 0.0,
                display_id: 0,
            }),
        }
    }

    #[test]
    fn drops_when_over_cap() {
        let mut rl = RateLimitedInjector::new(StubInjector::default(), 3);
        assert!(rl.try_inject(&mv(1)).unwrap());
        assert!(rl.try_inject(&mv(2)).unwrap());
        assert!(rl.try_inject(&mv(3)).unwrap());
        assert!(!rl.try_inject(&mv(4)).unwrap());
        assert!(!rl.try_inject(&mv(5)).unwrap());
        assert_eq!(rl.metrics().accepted, 3);
        assert_eq!(rl.metrics().dropped_rate_limit, 2);
        assert_eq!(rl.inner().recorded().len(), 3);
    }
}
