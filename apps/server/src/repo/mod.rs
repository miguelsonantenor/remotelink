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
    /// Refresh already used, revoked, expired, or unknown — map to HTTP 401.
    #[error("stale or invalid credential")]
    StaleCredential,
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

    /// Find non-revoked credential by access token hash (does not filter expiry).
    async fn find_by_access_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError>;

    /// Find non-revoked credential by refresh token hash (does not filter expiry).
    async fn find_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError>;

    /// Atomically consume a refresh credential and insert a replacement pair.
    ///
    /// Holds the storage lock / DB transaction for the whole rotate so concurrent
    /// refreshes with the same token cannot mint two live pairs.
    ///
    /// Returns [`RepoError::StaleCredential`] if the refresh is missing, already
    /// revoked, or past `expires_at`.
    async fn rotate_refresh(
        &self,
        refresh_token_hash: &str,
        new: NewCredential,
        now: DateTime<Utc>,
    ) -> Result<(Device, DeviceCredential), RepoError>;

    /// Revoke a single credential by id (no-op failure if already revoked → NotFound).
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
