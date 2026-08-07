//! Lightweight Prometheus-text metrics facade (no heavy `prometheus` crate).
//!
//! Process-wide registry for server `/metrics` and host/viewer `--metrics` dumps.
//! Metric names follow DESIGN.md observability: sessions, auth fails, input drops,
//! glass-latency placeholders, skew, ICE path.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Default histogram buckets for latency placeholders (milliseconds).
pub const LATENCY_BUCKETS_MS: &[f64] = &[
    5.0, 10.0, 20.0, 40.0, 50.0, 80.0, 100.0, 120.0, 180.0, 250.0, 500.0, 1000.0,
];

/// ICE path labels used by [`MetricsRegistry::inc_ice_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IcePath {
    /// Host / local candidate path.
    Host,
    /// Server-reflexive (STUN).
    Srflx,
    /// Relay (TURN).
    Relay,
}

impl IcePath {
    /// Prometheus label value.
    pub fn as_str(self) -> &'static str {
        match self {
            IcePath::Host => "host",
            IcePath::Srflx => "srflx",
            IcePath::Relay => "relay",
        }
    }

    /// Parse a candidate type string (`host` / `srflx` / `relay` / `prflx`→srflx).
    pub fn from_candidate_type(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "host" => Some(IcePath::Host),
            "srflx" | "prflx" => Some(IcePath::Srflx),
            "relay" => Some(IcePath::Relay),
            _ => None,
        }
    }
}

/// Session outcome labels for [`MetricsRegistry::inc_sessions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionResult {
    /// Host accepted the session.
    Accept,
    /// Host rejected the session.
    Reject,
    /// Session ended (hangup / timeout / kill).
    End,
}

impl SessionResult {
    /// Prometheus label value.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionResult::Accept => "accept",
            SessionResult::Reject => "reject",
            SessionResult::End => "end",
        }
    }
}

/// Atomic counter (monotonically increasing).
#[derive(Debug, Default)]
struct Counter {
    value: AtomicU64,
}

impl Counter {
    fn inc(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// Atomic gauge stored as f64 bits (i64 for atomics; set via bit-cast).
#[derive(Debug, Default)]
struct Gauge {
    bits: AtomicI64,
}

impl Gauge {
    fn set(&self, v: f64) {
        self.bits.store(v.to_bits() as i64, Ordering::Relaxed);
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed) as u64)
    }
}

/// Fixed-bucket histogram (cumulative counts + sum + count).
#[derive(Debug)]
struct Histogram {
    buckets: Vec<f64>,
    counts: Vec<AtomicU64>,
    sum_bits: AtomicI64,
    count: AtomicU64,
}

impl Histogram {
    fn new(buckets: &[f64]) -> Self {
        let mut b: Vec<f64> = buckets.to_vec();
        b.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        b.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        let n = b.len();
        Self {
            buckets: b,
            counts: (0..n).map(|_| AtomicU64::new(0)).collect(),
            sum_bits: AtomicI64::new(0f64.to_bits() as i64),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, value: f64) {
        for (i, upper) in self.buckets.iter().enumerate() {
            if value <= *upper {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        // +Inf is implied by `_count` in encode; still track all observations in count/sum.
        self.count.fetch_add(1, Ordering::Relaxed);
        // Best-effort float sum via CAS loop.
        let mut cur = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur as u64) + value).to_bits() as i64;
            match self.sum_bits.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }

    fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed) as u64)
    }

    fn observation_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Process-local metrics registry with Prometheus text exposition.
#[derive(Debug)]
pub struct MetricsRegistry {
    // Counters
    sessions: BTreeMap<&'static str, Counter>,
    auth_fail_total: Counter,
    input_events_total: Counter,
    input_drops_total: Counter,
    ice_path: BTreeMap<&'static str, Counter>,
    // Gauges
    skew_ms: Gauge,
    input_drop_rate: Gauge,
    // Histograms (placeholders until real glass timers land)
    glass_to_glass_ms: Histogram,
    input_to_glass_ms: Histogram,
    // Extra labeled counters for extensibility (mutex-protected map)
    extra_counters: Mutex<BTreeMap<String, u64>>,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    /// Empty registry with standard RemoteLink series pre-registered.
    pub fn new() -> Self {
        let mut sessions = BTreeMap::new();
        sessions.insert(SessionResult::Accept.as_str(), Counter::default());
        sessions.insert(SessionResult::Reject.as_str(), Counter::default());
        sessions.insert(SessionResult::End.as_str(), Counter::default());

        let mut ice_path = BTreeMap::new();
        ice_path.insert(IcePath::Host.as_str(), Counter::default());
        ice_path.insert(IcePath::Srflx.as_str(), Counter::default());
        ice_path.insert(IcePath::Relay.as_str(), Counter::default());

        Self {
            sessions,
            auth_fail_total: Counter::default(),
            input_events_total: Counter::default(),
            input_drops_total: Counter::default(),
            ice_path,
            skew_ms: Gauge::default(),
            input_drop_rate: Gauge::default(),
            glass_to_glass_ms: Histogram::new(LATENCY_BUCKETS_MS),
            input_to_glass_ms: Histogram::new(LATENCY_BUCKETS_MS),
            extra_counters: Mutex::new(BTreeMap::new()),
        }
    }

    /// Increment session outcome counter (`accept` / `reject` / `end`).
    pub fn inc_sessions(&self, result: SessionResult) {
        if let Some(c) = self.sessions.get(result.as_str()) {
            c.inc(1);
        }
    }

    /// Increment auth failure counter.
    pub fn inc_auth_fail(&self) {
        self.auth_fail_total.inc(1);
    }

    /// Record an input event that was accepted.
    pub fn inc_input_event(&self) {
        self.input_events_total.inc(1);
        self.refresh_input_drop_rate();
    }

    /// Record an input event dropped (host rate limit / queue full).
    pub fn inc_input_drop(&self) {
        self.input_drops_total.inc(1);
        self.refresh_input_drop_rate();
    }

    fn refresh_input_drop_rate(&self) {
        let drops = self.input_drops_total.get() as f64;
        let events = self.input_events_total.get() as f64;
        let total = drops + events;
        let rate = if total > 0.0 { drops / total } else { 0.0 };
        self.input_drop_rate.set(rate);
    }

    /// Set current A/V skew gauge (ms; positive = audio ahead).
    pub fn set_skew_ms(&self, skew_ms: f64) {
        self.skew_ms.set(skew_ms);
    }

    /// Observe glass-to-glass latency sample (ms). Placeholder until capture clocks land.
    pub fn observe_glass_to_glass_ms(&self, ms: f64) {
        self.glass_to_glass_ms.observe(ms);
    }

    /// Observe input-to-glass latency sample (ms). Placeholder until inject overlay lands.
    pub fn observe_input_to_glass_ms(&self, ms: f64) {
        self.input_to_glass_ms.observe(ms);
    }

    /// Increment ICE selected-path counter.
    pub fn inc_ice_path(&self, path: IcePath) {
        if let Some(c) = self.ice_path.get(path.as_str()) {
            c.inc(1);
        }
    }

    /// Increment a free-form counter (tests / future series). Name must be Prometheus-safe.
    pub fn inc_extra(&self, name: &str, n: u64) {
        if let Ok(mut m) = self.extra_counters.lock() {
            *m.entry(name.to_string()).or_insert(0) += n;
        }
    }

    /// Snapshot getters for tests.
    pub fn auth_fail_total(&self) -> u64 {
        self.auth_fail_total.get()
    }

    /// Sessions counter for `result`.
    pub fn sessions_total(&self, result: SessionResult) -> u64 {
        self.sessions
            .get(result.as_str())
            .map(|c| c.get())
            .unwrap_or(0)
    }

    /// Input drop rate gauge in `[0, 1]`.
    pub fn input_drop_rate(&self) -> f64 {
        self.input_drop_rate.get()
    }

    /// Current skew gauge.
    pub fn skew_ms(&self) -> f64 {
        self.skew_ms.get()
    }

    /// ICE path counter.
    pub fn ice_path_total(&self, path: IcePath) -> u64 {
        self.ice_path
            .get(path.as_str())
            .map(|c| c.get())
            .unwrap_or(0)
    }

    /// Encode the full registry as Prometheus text exposition format (0.0.4).
    pub fn encode_prometheus(&self) -> String {
        let mut out = String::with_capacity(2048);

        // sessions_total
        let _ = writeln!(
            out,
            "# HELP remotelink_sessions_total Total remote sessions by outcome"
        );
        let _ = writeln!(out, "# TYPE remotelink_sessions_total counter");
        for (label, c) in &self.sessions {
            let _ = writeln!(
                out,
                "remotelink_sessions_total{{result=\"{label}\"}} {}",
                c.get()
            );
        }

        // auth_fail_total
        let _ = writeln!(
            out,
            "# HELP remotelink_auth_fail_total Authentication failures (hello / token)"
        );
        let _ = writeln!(out, "# TYPE remotelink_auth_fail_total counter");
        let _ = writeln!(
            out,
            "remotelink_auth_fail_total {}",
            self.auth_fail_total.get()
        );

        // input events / drops / rate
        let _ = writeln!(
            out,
            "# HELP remotelink_input_events_total Input events accepted for injection"
        );
        let _ = writeln!(out, "# TYPE remotelink_input_events_total counter");
        let _ = writeln!(
            out,
            "remotelink_input_events_total {}",
            self.input_events_total.get()
        );

        let _ = writeln!(
            out,
            "# HELP remotelink_input_drops_total Input events dropped (rate limit / queue)"
        );
        let _ = writeln!(out, "# TYPE remotelink_input_drops_total counter");
        let _ = writeln!(
            out,
            "remotelink_input_drops_total {}",
            self.input_drops_total.get()
        );

        let _ = writeln!(
            out,
            "# HELP remotelink_input_drop_rate Fraction of input events dropped (0..1)"
        );
        let _ = writeln!(out, "# TYPE remotelink_input_drop_rate gauge");
        let _ = writeln!(
            out,
            "remotelink_input_drop_rate {}",
            format_float(self.input_drop_rate.get())
        );

        // skew_ms
        let _ = writeln!(
            out,
            "# HELP remotelink_skew_ms Current A/V skew in milliseconds (audio - video)"
        );
        let _ = writeln!(out, "# TYPE remotelink_skew_ms gauge");
        let _ = writeln!(
            out,
            "remotelink_skew_ms {}",
            format_float(self.skew_ms.get())
        );

        // ICE path
        let _ = writeln!(
            out,
            "# HELP remotelink_ice_path_total Selected ICE path type counts"
        );
        let _ = writeln!(out, "# TYPE remotelink_ice_path_total counter");
        for (label, c) in &self.ice_path {
            let _ = writeln!(
                out,
                "remotelink_ice_path_total{{path=\"{label}\"}} {}",
                c.get()
            );
        }

        // Glass latency placeholders
        encode_histogram(
            &mut out,
            "remotelink_glass_to_glass_ms",
            "Glass-to-glass video latency in milliseconds (placeholder until capture clocks)",
            &self.glass_to_glass_ms,
        );
        encode_histogram(
            &mut out,
            "remotelink_input_to_glass_ms",
            "Input-to-glass control-loop latency in milliseconds (placeholder)",
            &self.input_to_glass_ms,
        );

        if let Ok(extra) = self.extra_counters.lock() {
            for (name, v) in extra.iter() {
                if !is_safe_metric_name(name) {
                    continue;
                }
                let _ = writeln!(out, "# HELP {name} Extra counter");
                let _ = writeln!(out, "# TYPE {name} counter");
                let _ = writeln!(out, "{name} {v}");
            }
        }

        out
    }
}

fn encode_histogram(out: &mut String, name: &str, help: &str, h: &Histogram) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (i, upper) in h.buckets.iter().enumerate() {
        // counts[i] stores "observations <= upper" (all matching buckets incremented).
        let le = h.counts[i].load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"{}\"}} {le}", format_float(*upper));
    }
    let total = h.observation_count();
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {total}");
    let _ = writeln!(out, "{name}_sum {}", format_float(h.sum()));
    let _ = writeln!(out, "{name}_count {total}");
}

fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v.is_sign_positive() {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    // Compact but stable: avoid scientific for typical latency/skew ranges.
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".into()
    } else {
        s.to_string()
    }
}

fn is_safe_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Process-wide metrics registry (server `/metrics`, host/viewer dump).
pub fn process_registry() -> &'static MetricsRegistry {
    static REG: OnceLock<MetricsRegistry> = OnceLock::new();
    REG.get_or_init(MetricsRegistry::new)
}

/// Convenience: encode the process registry.
pub fn encode_process_metrics() -> String {
    process_registry().encode_prometheus()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_contains_required_series() {
        let reg = MetricsRegistry::new();
        reg.inc_sessions(SessionResult::Accept);
        reg.inc_sessions(SessionResult::Reject);
        reg.inc_auth_fail();
        reg.inc_input_event();
        reg.inc_input_event();
        reg.inc_input_drop();
        reg.set_skew_ms(12.5);
        reg.inc_ice_path(IcePath::Host);
        reg.inc_ice_path(IcePath::Srflx);
        reg.inc_ice_path(IcePath::Relay);
        reg.observe_glass_to_glass_ms(45.0);
        reg.observe_input_to_glass_ms(70.0);

        let text = reg.encode_prometheus();
        assert!(text.contains("# TYPE remotelink_sessions_total counter"));
        assert!(text.contains("remotelink_sessions_total{result=\"accept\"} 1"));
        assert!(text.contains("remotelink_sessions_total{result=\"reject\"} 1"));
        assert!(text.contains("remotelink_auth_fail_total 1"));
        assert!(text.contains("remotelink_input_events_total 2"));
        assert!(text.contains("remotelink_input_drops_total 1"));
        assert!(text.contains("remotelink_input_drop_rate"));
        assert!(text.contains("remotelink_skew_ms 12.5"));
        assert!(text.contains("remotelink_ice_path_total{path=\"host\"} 1"));
        assert!(text.contains("remotelink_ice_path_total{path=\"srflx\"} 1"));
        assert!(text.contains("remotelink_ice_path_total{path=\"relay\"} 1"));
        assert!(text.contains("# TYPE remotelink_glass_to_glass_ms histogram"));
        assert!(text.contains("remotelink_glass_to_glass_ms_count 1"));
        assert!(text.contains("remotelink_input_to_glass_ms_bucket{le=\"+Inf\"} 1"));
        // drop rate = 1/3
        let rate = reg.input_drop_rate();
        assert!((rate - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn encode_empty_registry_is_valid_shape() {
        let text = MetricsRegistry::new().encode_prometheus();
        assert!(text.starts_with("# HELP remotelink_sessions_total"));
        assert!(text.contains("remotelink_auth_fail_total 0"));
        assert!(text.contains("remotelink_skew_ms 0"));
        // Every series has HELP + TYPE
        assert_eq!(
            text.matches("# HELP ").count(),
            text.matches("# TYPE ").count()
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let reg = MetricsRegistry::new();
        reg.observe_glass_to_glass_ms(5.0);
        reg.observe_glass_to_glass_ms(100.0);
        let text = reg.encode_prometheus();
        // le="5" should see 1; le="100" should see 2; +Inf = 2
        assert!(text.contains("remotelink_glass_to_glass_ms_bucket{le=\"5\"} 1"));
        assert!(text.contains("remotelink_glass_to_glass_ms_bucket{le=\"100\"} 2"));
        assert!(text.contains("remotelink_glass_to_glass_ms_bucket{le=\"+Inf\"} 2"));
        assert!(text.contains("remotelink_glass_to_glass_ms_count 2"));
    }

    #[test]
    fn ice_path_from_candidate_type() {
        assert_eq!(IcePath::from_candidate_type("host"), Some(IcePath::Host));
        assert_eq!(IcePath::from_candidate_type("SRFLX"), Some(IcePath::Srflx));
        assert_eq!(IcePath::from_candidate_type("prflx"), Some(IcePath::Srflx));
        assert_eq!(IcePath::from_candidate_type("relay"), Some(IcePath::Relay));
        assert_eq!(IcePath::from_candidate_type("unknown"), None);
    }

    #[test]
    fn process_registry_is_singleton() {
        let a = process_registry() as *const MetricsRegistry;
        let b = process_registry() as *const MetricsRegistry;
        assert_eq!(a, b);
    }

    #[test]
    fn extra_counter_and_safe_name_filter() {
        let reg = MetricsRegistry::new();
        reg.inc_extra("remotelink_custom_total", 3);
        reg.inc_extra("bad name!", 9);
        let text = reg.encode_prometheus();
        assert!(text.contains("remotelink_custom_total 3"));
        assert!(!text.contains("bad name"));
    }
}
