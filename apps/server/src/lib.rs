//! RemoteLink signaling / device-registry HTTP server.
//!
//! # Endpoints
//!
//! - `POST /v1/devices/register` — enroll host, store pubkey + public_id, issue tokens
//! - `POST /v1/devices/{id}/token/refresh` — rotate device credentials
//! - `DELETE /v1/devices/{id}` — soft-delete + revoke credentials
//! - `POST /v1/devices/{id}/otp` — host-authenticated Mode A OTP hash mint (TTL)
//! - `POST|GET /v1/devices/{id}/blocklist` — host blocklist of viewers/IPs
//! - `GET /v1/devices/{id}/blocklist/check` — check if a subject is blocked
//! - `GET /v1/devices/{id}/audit` — host owner audit list
//! - `GET /v1/ws` — signaling WebSocket (`hello`, `session_intent`, accept/reject)
//! - `GET /healthz` / `GET /readyz` — probes
//!
//! Storage is abstracted via [`repo::DeviceRepository`] with in-memory and
//! Postgres implementations. Session presence/busy-lock lives in
//! [`session::SessionRegistry`] (in-memory; Redis later). Security controls
//! ([`security`]) provide in-memory rate limits, auth-attempt lockout, audit,
//! and blocklist. Mode A OTP hashes live in [`otp::MemoryOtpStore`]. Unit tests
//! use the memory backends.

pub mod credentials;
pub mod error;
pub mod models;
pub mod otp;
pub mod repo;
pub mod routes;
pub mod security;
pub mod session;
pub mod state;

pub use error::{AppError, AppResult, ErrorBody};
pub use models::{Device, DeviceCredential, DeviceStatus, IssuedTokens, NewCredential, NewDevice};
pub use otp::{
    MemoryOtpStore, OtpMintRequest, OtpMintResponse, OtpPrefilterResult, OtpStoreError,
    DEFAULT_OTP_PEPPER, DEFAULT_OTP_TTL_SECS,
};
pub use repo::{DeviceRepository, MemoryDeviceRepo, PostgresDeviceRepo, RepoError};
pub use routes::{router, router_with_repo};
pub use security::{
    hash_subject, AuditEvent, AuditEventType, AuditStore, AuthAttemptTracker, BlockSubjectType,
    BlocklistEntry, BlocklistStore, ClientIpConfig, MemoryAuditStore, MemoryBlocklist,
    NewBlocklistEntry, RateLimitConfig, RateLimiter, RateLimiters,
};
pub use session::{SessionRegistry, SessionState, SharedSessionRegistry};
pub use state::AppState;

/// Crate / workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
