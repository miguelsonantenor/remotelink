//! Error types for the auth package.

use thiserror::Error;

/// Errors produced by auth helpers.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Device public ID failed check-digit or format validation.
    #[error("invalid device public ID: {0}")]
    InvalidDeviceId(String),

    /// OTP plaintext failed format checks (digit count / non-numeric).
    #[error("invalid OTP format: {0}")]
    InvalidOtpFormat(String),

    /// OTP verification failed (wrong code, expired, or already consumed).
    #[error("OTP verification failed: {0}")]
    OtpVerify(String),

    /// Challenge-response MAC verification failed.
    #[error("challenge MAC verification failed")]
    ChallengeMacMismatch,

    /// Password hashing or verification failed.
    #[error("password operation failed: {0}")]
    Password(String),

    /// Fingerprint signature verification failed.
    #[error("fingerprint signature verification failed")]
    FingerprintSigInvalid,

    /// Post-DTLS DataChannel identity bind failed.
    #[error("identity bind failed: {0}")]
    IdentityBind(String),

    /// Session is not authorized (Mode A/B) yet.
    #[error("session not authorized")]
    SessionNotAuthorized,

    /// Input rejected: identity not bound and/or session not authorized.
    #[error("input not allowed: identity_bound={identity_bound} session_authorized={session_authorized}")]
    InputNotAllowed {
        /// Whether DTLS/fingerprint + DC challenge completed.
        identity_bound: bool,
        /// Whether Mode A/B session auth succeeded.
        session_authorized: bool,
    },

    /// Cryptographic / key material error.
    #[error("crypto error: {0}")]
    Crypto(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, AuthError>;
