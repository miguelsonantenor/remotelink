//! Shared types and utilities for RemoteLink crates.
//!
//! Includes a lightweight Prometheus-text metrics facade and `session_id`
//! tracing helpers used by server, host, and viewer.

pub mod metrics;
pub mod tracing_session;

pub use metrics::{
    encode_process_metrics, process_registry, IcePath, MetricsRegistry, SessionResult,
    LATENCY_BUCKETS_MS,
};
pub use tracing_session::{session_span, session_span_at, SESSION_ID_FIELD};

/// Crate version from `Cargo.toml` (`CARGO_PKG_VERSION`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Common result type used across RemoteLink packages.
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn result_ok_roundtrips() {
        let value: Result<i32> = Ok(42);
        assert!(matches!(value, Ok(42)));
    }
}
