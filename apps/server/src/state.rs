//! Shared application state.

use std::sync::Arc;

use crate::repo::DeviceRepository;
use crate::session::{SessionRegistry, SharedSessionRegistry};

/// Axum state: repository + in-memory session/presence registry.
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn DeviceRepository>,
    pub sessions: SharedSessionRegistry,
}

impl AppState {
    pub fn new(repo: Arc<dyn DeviceRepository>) -> Self {
        Self {
            repo,
            sessions: Arc::new(SessionRegistry::new()),
        }
    }

    /// Construct with an explicit session registry (tests / multi-node later).
    pub fn with_sessions(repo: Arc<dyn DeviceRepository>, sessions: SharedSessionRegistry) -> Self {
        Self { repo, sessions }
    }
}
