//! RemoteLink signaling / device-registry HTTP server.
//!
//! # Endpoints (PR 4)
//!
//! - `POST /v1/devices/register` — enroll host, store pubkey + public_id, issue tokens
//! - `POST /v1/devices/{id}/token/refresh` — rotate device credentials
//! - `DELETE /v1/devices/{id}` — soft-delete + revoke credentials
//! - `GET /healthz` / `GET /readyz` — probes
//!
//! Storage is abstracted via [`repo::DeviceRepository`] with in-memory and
//! Postgres implementations. Unit tests use the memory backend.

pub mod credentials;
pub mod error;
pub mod models;
pub mod repo;
pub mod routes;
pub mod state;

pub use error::{AppError, AppResult, ErrorBody};
pub use models::{Device, DeviceCredential, DeviceStatus, IssuedTokens, NewCredential, NewDevice};
pub use repo::{DeviceRepository, MemoryDeviceRepo, PostgresDeviceRepo, RepoError};
pub use routes::{router, router_with_repo};
pub use state::AppState;

/// Crate / workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
