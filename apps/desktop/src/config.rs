//! Persist desktop shell settings under the user config directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default lab signaling endpoint (self-hosted / local server).
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:18080";

/// User-facing app settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Signaling HTTP base (hidden under Advanced by default).
    pub server: String,
    /// Display name used when enrolling this PC as a host.
    pub display_name: String,
    /// Start host service when the app opens.
    pub auto_start_host: bool,
    /// Register this app in the current-user Windows startup list.
    #[serde(default)]
    pub start_with_windows: bool,
    /// Transport mode string: webrtc | live | mock.
    pub transport: String,
    /// Optional STUN/TURN URLs (`stun:host:3478`, comma-separated). Empty = host ICE only.
    #[serde(default)]
    pub stun_urls: String,
    /// Recent remote host IDs (most recent first).
    pub recent_hosts: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: std::env::var("REMOTELINK_SERVER")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_SERVER.into()),
            display_name: hostname_guess(),
            auto_start_host: true,
            start_with_windows: false,
            transport: "webrtc".into(),
            stun_urls: String::new(),
            recent_hosts: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Directory for config, status, and host credentials used by the shell.
    pub fn data_dir() -> PathBuf {
        if let Ok(p) = std::env::var("REMOTELINK_DATA_DIR") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(base).join("RemoteLink");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".remotelink");
        }
        PathBuf::from(".remotelink")
    }

    /// Path to `config.json`.
    pub fn config_path() -> PathBuf {
        Self::data_dir().join("config.json")
    }

    /// Path for host status JSON (polled by the UI).
    pub fn status_path() -> PathBuf {
        Self::data_dir().join("host-status.json")
    }

    /// Path for host credential file managed by the in-process host.
    pub fn creds_path() -> PathBuf {
        Self::data_dir().join("host-creds.json")
    }

    /// Load from disk or defaults.
    pub fn load() -> Self {
        let path = Self::config_path();
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("remotelink-app: config parse failed ({e}); using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Create data dir and write config.
    pub fn save(&self) -> Result<(), String> {
        let dir = Self::data_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("create data dir: {e}"))?;
        let path = Self::config_path();
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&path, text).map_err(|e| format!("write config: {e}"))
    }

    /// Remember a remote host ID (deduped, cap 12).
    pub fn push_recent(&mut self, host_id: &str) {
        let id = host_id.trim();
        if id.is_empty() {
            return;
        }
        self.recent_hosts.retain(|h| h != id);
        self.recent_hosts.insert(0, id.to_string());
        self.recent_hosts.truncate(12);
    }
}

fn hostname_guess() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "This PC".into())
}

/// Ensure parent directory exists for a path.
pub fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    Ok(())
}
