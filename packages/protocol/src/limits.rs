//! Payload size limits for signaling fields.

/// Maximum allowed SDP string length in bytes (UTF-8).
pub const MAX_SDP_BYTES: usize = 64 * 1024;

/// Maximum allowed ICE candidate JSON object serialized size in bytes.
pub const MAX_ICE_CANDIDATE_BYTES: usize = 4 * 1024;

/// Maximum allowed fingerprint signature string length in bytes.
pub const MAX_FINGERPRINT_SIG_BYTES: usize = 8 * 1024;

/// Maximum allowed opaque auth/stats payload serialized size in bytes.
pub const MAX_OPAQUE_PAYLOAD_BYTES: usize = 16 * 1024;
