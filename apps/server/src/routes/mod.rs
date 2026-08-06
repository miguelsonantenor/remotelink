//! HTTP route handlers.

mod devices;
mod health;
mod ws;

use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::Router;

use crate::repo::DeviceRepository;
use crate::state::AppState;

pub use devices::{
    delete_device, refresh_token, register_device, DeleteParams, RefreshRequest, RegisterRequest,
    RegisterResponse, TokenResponse,
};
pub use health::{healthz, readyz};
pub use ws::ws_handler;

/// Build the full HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/devices/register", post(register_device))
        .route("/v1/devices/{id}/token/refresh", post(refresh_token))
        .route("/v1/devices/{id}", delete(delete_device))
        .route("/v1/ws", get(ws_handler))
        .with_state(state)
}

/// Convenience builder with an `Arc` repository (memory or postgres).
pub fn router_with_repo(repo: Arc<dyn DeviceRepository>) -> Router {
    router(AppState::new(repo))
}
