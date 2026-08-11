//! Decode/encode error types for protocol helpers.

use std::fmt;

/// Errors from protocol encode/decode helpers.
#[derive(Debug)]
pub enum ProtocolError {
    /// Underlying JSON (de)serialization failure.
    Json(serde_json::Error),
    /// A field exceeded its documented size limit.
    PayloadTooLarge {
        field: &'static str,
        len: usize,
        max: usize,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "json error: {e}"),
            Self::PayloadTooLarge { field, len, max } => {
                write!(f, "payload too large: {field} is {len} bytes (max {max})")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(e) => Some(e),
            Self::PayloadTooLarge { .. } => None,
        }
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl ProtocolError {
    pub(crate) fn too_large(field: &'static str, len: usize, max: usize) -> Self {
        Self::PayloadTooLarge { field, len, max }
    }
}
