//! Viewer-core errors.

use thiserror::Error;

/// Errors from the toolkit-agnostic viewer session and helpers.
#[derive(Debug, Error)]
pub enum ViewerError {
    /// Invalid connection credentials or connect request.
    #[error("invalid connect: {0}")]
    InvalidConnect(String),

    /// Session is not in a state that allows the requested operation.
    #[error("invalid state: expected {expected}, actual {actual}")]
    InvalidState {
        /// Expected phase / condition.
        expected: &'static str,
        /// Actual phase / condition.
        actual: String,
    },

    /// Peer transport failure.
    #[error("transport: {0}")]
    Transport(#[from] remotelink_net::NetError),

    /// Protocol encode/decode failure.
    #[error("protocol: {0}")]
    Protocol(#[from] remotelink_protocol::ProtocolError),

    /// Auth / identity bind failure (fingerprint_sig or DC challenge).
    #[error("auth: {0}")]
    Auth(#[from] remotelink_auth::AuthError),

    /// Media decode / playout failure.
    #[error("media: {0}")]
    Media(String),

    /// Internal invariant broken.
    #[error("internal: {0}")]
    Internal(String),
}

/// Result alias for viewer-core.
pub type Result<T> = std::result::Result<T, ViewerError>;
