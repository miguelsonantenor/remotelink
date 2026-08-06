//! RemoteLink signaling / device-registry HTTP server.
//!
//! # Endpoints
//!
//! - `POST /v1/devices/register` — enroll host, store pubkey + public_id, issue tokens
//! - `POST /v1/devices/{id}/token/refresh` — rotate device credentials
//! - `DELETE /v1/devices/{id}` — soft-delete + revoke credentials
//! - `GET /v1/ws` — signaling WebSocket (hello, session lifecycle, SDP/ICE relay)
//! - `GET /healthz` / `GET /readyz` — probes
//!
//! Storage is abstracted via [`repo::DeviceRepository`] with in-memory and
//! Postgres implementations. Session presence/busy-lock lives in
//! [`session::SessionRegistry`] (in-memory; Redis later). Unit tests use the
//! memory backend.

pub mod credentials;
pub mod error;
pub mod models;
pub mod repo;
pub mod routes;
pub mod session;
pub mod state;

pub use error::{AppError, AppResult, ErrorBody};
pub use models::{Device, DeviceCredential, DeviceStatus, IssuedTokens, NewCredential, NewDevice};
pub use repo::{DeviceRepository, MemoryDeviceRepo, PostgresDeviceRepo, RepoError};
pub use routes::{router, router_with_repo};
pub use session::{SessionRegistry, SessionState, SharedSessionRegistry};
pub use state::AppState;

/// Crate / workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
