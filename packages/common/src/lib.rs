//! Shared types and utilities for RemoteLink crates.
//!
//! - Metrics facade (Prometheus text)
//! - `session_id` tracing helpers
//! - Signed update manifest schema + verify

pub mod metrics;
pub mod tracing_session;
pub mod update_manifest;

pub use metrics::{
    encode_process_metrics, process_registry, IcePath, MetricsRegistry, SessionResult,
    LATENCY_BUCKETS_MS,
};
pub use tracing_session::{session_span, session_span_at, SESSION_ID_FIELD};
pub use update_manifest::{
    encode_manifest_message, generate_manifest_keypair, parse_signed_manifest, sign_manifest,
    signed_manifest_to_json, signing_key_from_bytes, verify_manifest, verify_manifest_for_channel,
    verifying_key_from_bytes, ManifestArtifact, ManifestError, ManifestResult,
    SignedUpdateManifest, UpdateChannel, UpdateManifest, MANIFEST_DOMAIN, MANIFEST_SCHEMA_VERSION,
};

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
