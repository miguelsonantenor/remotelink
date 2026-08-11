//! Host credential file persistence for restart-friendly lab/service deploys.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::register::{DeviceRegistration, RegisterError};

/// Default relative path for host credentials (cwd of the process).
pub const DEFAULT_CREDS_PATH: &str = ".remotelink-host.json";

/// Persisted host enrollment material (lab secrets — protect the file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostCredentialFile {
    /// Schema version for forward compatibility.
    #[serde(default = "creds_schema_v1")]
    pub schema: u32,
    /// Signaling HTTP base used at enrollment.
    pub server: String,
    /// Device public id.
    pub public_id: String,
    /// Optional display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Access token for WSS hello / HTTP bearer.
    pub access_token: String,
    /// Refresh token for rotation.
    pub refresh_token: String,
    /// Access token expiry (RFC3339) when known.
    #[serde(default)]
    pub expires_at: Option<String>,
}

fn creds_schema_v1() -> u32 {
    1
}

impl HostCredentialFile {
    /// Build from a fresh registration response.
    pub fn from_registration(server: &str, reg: &DeviceRegistration) -> Self {
        Self {
            schema: 1,
            server: server.trim_end_matches('/').to_string(),
            public_id: reg.public_id.clone(),
            display_name: reg.display_name.clone(),
            access_token: reg.access_token.clone(),
            refresh_token: reg.refresh_token.clone(),
            expires_at: Some(reg.expires_at.clone()),
        }
    }

    /// Resolve path: explicit → `REMOTELINK_HOST_CREDS` → default.
    pub fn resolve_path(explicit: Option<&Path>) -> PathBuf {
        if let Some(p) = explicit {
            return p.to_path_buf();
        }
        if let Ok(p) = std::env::var("REMOTELINK_HOST_CREDS") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        PathBuf::from(DEFAULT_CREDS_PATH)
    }

    /// Load credentials from disk.
    pub fn load(path: &Path) -> Result<Self, RegisterError> {
        let text = fs::read_to_string(path).map_err(|e| {
            RegisterError::Http(format!("read creds {}: {e}", path.display()))
        })?;
        serde_json::from_str(&text).map_err(|e| RegisterError::Parse(format!("creds json: {e}")))
    }

    /// Atomic-ish write (write temp then rename when possible).
    pub fn save(&self, path: &Path) -> Result<(), RegisterError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    RegisterError::Http(format!("create creds dir {}: {e}", parent.display()))
                })?;
            }
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| RegisterError::Parse(format!("encode creds: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text.as_bytes())
            .map_err(|e| RegisterError::Http(format!("write creds tmp: {e}")))?;
        fs::rename(&tmp, path).or_else(|_| {
            // Windows: rename over existing can fail; write directly.
            fs::write(path, text.as_bytes())
                .map_err(|e| RegisterError::Http(format!("write creds: {e}")))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn roundtrip_tempfile() {
        let mut path = std::env::temp_dir();
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!("remotelink-creds-{n}.json"));

        let c = HostCredentialFile {
            schema: 1,
            server: "http://127.0.0.1:8080".into(),
            public_id: "1234567890".into(),
            display_name: Some("lab".into()),
            access_token: "rl_at_x".into(),
            refresh_token: "rl_rt_y".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
        };
        c.save(&path).unwrap();
        let loaded = HostCredentialFile::load(&path).unwrap();
        assert_eq!(loaded, c);
        let _ = fs::remove_file(&path);
    }
}
