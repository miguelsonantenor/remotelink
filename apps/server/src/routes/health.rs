//! Liveness and readiness probes.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// Probe response body.
#[derive(Serialize)]
pub struct HealthBody {
    /// Fixed status string (`ok` / `ready`).
    pub status: &'static str,
}

/// Liveness: process is up.
pub async fn healthz() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

/// Readiness: storage backend is reachable.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<HealthBody>, StatusCode> {
    state
        .repo
        .ping()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(HealthBody { status: "ready" }))
}
