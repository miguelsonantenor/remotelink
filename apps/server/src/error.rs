//! HTTP and repository error types.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Application-level errors mapped to HTTP responses.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Rate limit or auth lockout; optional retry-after seconds for `Retry-After`.
    #[error("{message}")]
    RateLimited {
        message: String,
        retry_after_secs: u64,
    },
    /// Server-side failure; client sees a generic message (detail is logged).
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn rate_limited(message: impl Into<String>, retry_after: std::time::Duration) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after_secs: retry_after.as_secs().max(1),
        }
    }
}

/// JSON error body.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
}

impl AppError {
    fn code_and_status(&self) -> (&'static str, StatusCode) {
        match self {
            Self::BadRequest(_) => ("bad_request", StatusCode::BAD_REQUEST),
            Self::Unauthorized => ("unauthorized", StatusCode::UNAUTHORIZED),
            Self::NotFound => ("not_found", StatusCode::NOT_FOUND),
            Self::Conflict(_) => ("conflict", StatusCode::CONFLICT),
            Self::Forbidden(_) => ("forbidden", StatusCode::FORBIDDEN),
            Self::RateLimited { .. } => ("rate_limited", StatusCode::TOO_MANY_REQUESTS),
            Self::Internal(_) => ("internal", StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    /// Client-facing message (no raw backend details for 500s).
    fn client_message(&self) -> String {
        match self {
            Self::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, status) = self.code_and_status();
        if matches!(self, Self::Internal(_)) {
            tracing::error!(error = %self, "internal server error");
        }
        let retry_after = match &self {
            Self::RateLimited {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        };
        let body = ErrorBody {
            error: code.to_string(),
            message: self.client_message(),
        };
        let mut res = (status, Json(body)).into_response();
        if let Some(secs) = retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        res
    }
}

impl From<crate::repo::RepoError> for AppError {
    fn from(value: crate::repo::RepoError) -> Self {
        match value {
            crate::repo::RepoError::NotFound => Self::NotFound,
            crate::repo::RepoError::Conflict(msg) => Self::Conflict(msg),
            crate::repo::RepoError::StaleCredential => Self::Unauthorized,
            crate::repo::RepoError::Internal(msg) => Self::Internal(msg),
        }
    }
}

impl From<crate::security::BlocklistError> for AppError {
    fn from(value: crate::security::BlocklistError) -> Self {
        match value {
            crate::security::BlocklistError::NotFound => Self::NotFound,
            crate::security::BlocklistError::Conflict(msg) => Self::Conflict(msg),
            crate::security::BlocklistError::Internal(msg) => Self::Internal(msg),
        }
    }
}

impl From<crate::security::AuditError> for AppError {
    fn from(value: crate::security::AuditError) -> Self {
        match value {
            crate::security::AuditError::Internal(msg) => Self::Internal(msg),
        }
    }
}

/// Convenience result for handlers.
pub type AppResult<T> = Result<T, AppError>;
