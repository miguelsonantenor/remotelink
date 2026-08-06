//! Device registry repository abstraction.

mod memory;
mod postgres;

pub use memory::MemoryDeviceRepo;
pub use postgres::PostgresDeviceRepo;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::models::{Device, DeviceCredential, NewCredential, NewDevice};

/// Repository errors.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Async storage for devices and credentials.
#[async_trait]
pub trait DeviceRepository: Send + Sync {
    /// Insert a new device; returns the stored row with assigned id.
    async fn create_device(&self, new: NewDevice) -> Result<Device, RepoError>;

    /// Lookup by public ID (includes deleted/disabled).
    async fn get_by_public_id(&self, public_id: &str) -> Result<Option<Device>, RepoError>;

    /// Lookup by internal id.
    async fn get_by_id(&self, id: i64) -> Result<Option<Device>, RepoError>;

    /// Soft-delete device (status=deleted, deleted_at set). Returns false if missing.
    async fn soft_delete(&self, public_id: &str, at: DateTime<Utc>) -> Result<bool, RepoError>;

    /// Insert credential hashes for a device.
    async fn insert_credential(&self, new: NewCredential) -> Result<DeviceCredential, RepoError>;

    /// Find non-revoked credential by access token hash.
    async fn find_by_access_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError>;

    /// Find non-revoked credential by refresh token hash.
    async fn find_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError>;

    /// Revoke a single credential by id.
    async fn revoke_credential(
        &self,
        credential_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError>;

    /// Revoke all credentials for a device.
    async fn revoke_all_for_device(
        &self,
        device_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError>;

    /// Touch last_seen_at.
    async fn touch_last_seen(&self, device_id: i64, at: DateTime<Utc>) -> Result<(), RepoError>;

    /// Backend readiness (e.g. `SELECT 1`).
    async fn ping(&self) -> Result<(), RepoError>;
}
