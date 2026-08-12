//! User-login auto-start (Windows HKCU Run key).

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

/// Registry value name under the current-user Run key.
#[cfg(windows)]
pub const RUN_VALUE_NAME: &str = "RemoteLink";

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Command line written to the Run key: quoted exe + `--autostart`.
#[cfg(windows)]
pub fn run_command_line(exe: &Path) -> String {
    format!("\"{}\" --autostart", exe.display())
}

/// Path of this process, if it looks like a real binary.
pub fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("current exe: {e}"))
}

/// Whether the current-user Run key has our value.
#[allow(dead_code)]
pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        Command::new("reg")
            .args(["query", RUN_KEY, "/v", RUN_VALUE_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Add or refresh the login Run entry for `exe`.
pub fn enable(exe: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let value = run_command_line(exe);
        let out = Command::new("reg")
            .args([
                "add",
                RUN_KEY,
                "/v",
                RUN_VALUE_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .output()
            .map_err(|e| format!("reg add: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "could not enable Start with Windows: {}",
                stderr.trim()
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err("Start with Windows is only available on Windows".into())
    }
}

/// Remove the login Run entry (ok if it was already gone).
pub fn disable() -> Result<(), String> {
    #[cfg(windows)]
    {
        let out = Command::new("reg")
            .args(["delete", RUN_KEY, "/v", RUN_VALUE_NAME, "/f"])
            .output()
            .map_err(|e| format!("reg delete: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
            if stderr.contains("unable to find") || stderr.contains("cannot find") {
                return Ok(());
            }
            return Err(format!(
                "could not disable Start with Windows: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Apply config: enable with the current exe, or disable.
pub fn apply(enabled: bool) -> Result<(), String> {
    if enabled {
        enable(&current_exe()?)
    } else {
        disable()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn run_command_quotes_exe_and_adds_flag() {
        let p = PathBuf::from(r"C:\Program Files\RemoteLink\remotelink-app.exe");
        assert_eq!(
            run_command_line(&p),
            r#""C:\Program Files\RemoteLink\remotelink-app.exe" --autostart"#
        );
    }
}
