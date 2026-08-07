//! Prometheus metrics exposition (`GET /metrics`).

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use remotelink_common::encode_process_metrics;

/// `GET /metrics` — Prometheus text exposition (0.0.4).
pub async fn metrics() -> impl IntoResponse {
    let body = encode_process_metrics();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (StatusCode::OK, headers, body)
}
