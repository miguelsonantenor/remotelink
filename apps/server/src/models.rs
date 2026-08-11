//! Domain models for device registry and credentials.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Device lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Active,
    Disabled,
    Deleted,
}

impl DeviceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// Enrolled host device row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    pub public_id: String,
    pub display_name: Option<String>,
    pub public_key: Vec<u8>,
    pub password_hash: Option<String>,
    pub protocol_version_last: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub status: DeviceStatus,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Stored credential hashes (plaintext tokens never persisted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCredential {
    pub id: i64,
    pub device_id: i64,
    pub token_hash: String,
    pub refresh_token_hash: String,
    /// When the access token stops being accepted for bearer authz.
    pub access_expires_at: DateTime<Utc>,
    /// When the refresh token / credential row expires.
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Fields required to insert a new device.
#[derive(Debug, Clone)]
pub struct NewDevice {
    pub public_id: String,
    pub display_name: Option<String>,
    pub public_key: Vec<u8>,
    pub protocol_version_last: Option<i32>,
}

/// Fields required to insert credential hashes.
#[derive(Debug, Clone)]
pub struct NewCredential {
    pub device_id: i64,
    pub token_hash: String,
    pub refresh_token_hash: String,
    pub access_expires_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Issued opaque tokens (returned once to the client).
#[derive(Debug, Clone)]
pub struct IssuedTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Access-token expiry (matches `access_expires_at` stored server-side).
    pub expires_at: DateTime<Utc>,
}
