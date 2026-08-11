//! Read host tray status JSON written by `remotelink-host` / in-process host.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use serde::Deserialize;

/// Snapshot of host status for the product UI.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // fields mirror host status JSON; UI uses a subset today
pub struct HostStatusSnapshot {
    /// Enrollment display name.
    #[serde(default)]
    pub display_name: String,
    /// 10-digit public id when enrolled.
    pub public_id: Option<String>,
    /// Mode A OTP plaintext (host-local).
    pub otp_code: Option<String>,
    /// OTP expiry string when known.
    pub otp_expires_at: Option<String>,
    /// `Active` / `Inactive`.
    #[serde(default)]
    pub chrome: String,
    /// Viewer session id when active.
    pub session_id: Option<String>,
    /// Optional viewer label.
    pub viewer_label: Option<String>,
    /// Session connected flag.
    #[serde(default)]
    pub connected: bool,
    /// Session active flag.
    #[serde(default)]
    pub active: bool,
    /// Last tray event.
    pub last_event: Option<String>,
    /// Tooltip text.
    #[serde(default)]
    pub tooltip: String,
}

/// Load status file if present and parseable.
pub fn read_status(path: &Path) -> Option<HostStatusSnapshot> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Age of the status file in seconds (None if missing).
pub fn status_age_secs(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let now = SystemTime::now();
    now.duration_since(modified).ok().map(|d| d.as_secs())
}
