//! Shared application state.

use std::sync::Arc;

use crate::repo::DeviceRepository;

/// Axum state: repository trait object (memory or postgres).
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn DeviceRepository>,
}

impl AppState {
    pub fn new(repo: Arc<dyn DeviceRepository>) -> Self {
        Self { repo }
    }
}
