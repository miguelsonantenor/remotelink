//! HTTP and repository error types.

use axum::http::StatusCode;
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
    #[error("internal error: {0}")]
    Internal(String),
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
            Self::Internal(_) => ("internal", StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, status) = self.code_and_status();
        let body = ErrorBody {
            error: code.to_string(),
            message: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

impl From<crate::repo::RepoError> for AppError {
    fn from(value: crate::repo::RepoError) -> Self {
        match value {
            crate::repo::RepoError::NotFound => Self::NotFound,
            crate::repo::RepoError::Conflict(msg) => Self::Conflict(msg),
            crate::repo::RepoError::Internal(msg) => Self::Internal(msg),
        }
    }
}

/// Convenience result for handlers.
pub type AppResult<T> = Result<T, AppError>;
