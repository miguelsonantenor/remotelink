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

    /// Cryptographic / key material error.
    #[error("crypto error: {0}")]
    Crypto(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, AuthError>;
