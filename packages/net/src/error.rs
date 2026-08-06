//! Errors for the net / PeerTransport layer.

use thiserror::Error;

/// Transport-layer error returned by [`crate::PeerTransport`] operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NetError {
    /// SDP or ICE description is invalid for the current peer state.
    #[error("invalid session description: {0}")]
    InvalidDescription(String),

    /// ICE candidate could not be applied.
    #[error("invalid ICE candidate: {0}")]
    InvalidCandidate(String),

    /// DTLS fingerprint is not a valid SHA-256 digest form.
    #[error("invalid DTLS fingerprint: {0}")]
    InvalidFingerprint(String),

    /// Peer is not in a state that accepts this operation.
    #[error("invalid state: expected {expected}, was {actual}")]
    InvalidState {
        /// Expected state description.
        expected: &'static str,
        /// Actual state description.
        actual: String,
    },

    /// Media or data send failed (not connected, closed, or backpressure).
    #[error("send failed: {0}")]
    SendFailed(String),

    /// Transport is closed.
    #[error("transport closed")]
    Closed,

    /// Feature / backend not available in this build.
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    /// Generic internal error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Result alias for net operations.
pub type Result<T> = std::result::Result<T, NetError>;
