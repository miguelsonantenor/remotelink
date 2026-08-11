//! Background host service for "this PC" (allow remote access).
//!
//! Runs `remotelink-host` as a **child process** so WSS + media stay alive
//! independently of the egui UI thread (in-process tokio+COM was dropping
//! offline after OTP mint under the desktop shell).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use remotelink_net::TransportMode;

/// Lifecycle handle for the host service child process.
pub struct HostWorker {
    child: Option<Child>,
    last_error: Arc<Mutex<Option<String>>>,
    host_exe: PathBuf,
}

impl HostWorker {
    /// Spawn `remotelink-host --role=service` with status + creds under the data dir.
    pub fn start(
        server: String,
        display_name: String,
        transport: TransportMode,
        status_path: PathBuf,
        creds_path: PathBuf,
    ) -> Result<Self, String> {
        let host_exe = resolve_host_exe()?;
        let last_error = Arc::new(Mutex::new(None));

        // Log file next to status for support / debugging.
        let log_path = status_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("host-service.log");
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("open host log {}: {e}", log_path.display()))?;
        let log_err = log_file
            .try_clone()
            .map_err(|e| format!("clone host log: {e}"))?;

        let mut cmd = Command::new(&host_exe);
        cmd.arg("--role=service")
            .arg(format!("--server={server}"))
            .arg(format!("--transport={}", transport.as_str()))
            .arg(format!("--display-name={display_name}"))
            .arg(format!("--creds={}", creds_path.display()))
            .arg(format!("--status-path={}", status_path.display()))
            .arg("--sessions=0")
            .arg("--reconnect")
            .arg("--tray")
            .arg("--no-os-tray")
            .arg("--mint-otp")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .stdin(Stdio::null());

        // Hide console window on Windows (host is a console binary).
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd.spawn().map_err(|e| {
            format!(
                "failed to start host `{}`: {e}. Build with `cargo build -p remotelink-host` \
                 or set REMOTELINK_HOST_EXE.",
                host_exe.display()
            )
        })?;

        eprintln!(
            "remotelink-app: started host pid={} exe={} server={server} log={}",
            child.id(),
            host_exe.display(),
            log_path.display()
        );

        Ok(Self {
            child: Some(child),
            last_error,
            host_exe,
        })
    }

    /// Path of the host binary that was launched.
    pub fn host_exe(&self) -> &Path {
        &self.host_exe
    }

    /// Non-blocking poll: reaps exit status and records errors.
    /// Returns true if the host process is still running.
    pub fn poll(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                let msg = format!("host process exited ({status})");
                eprintln!("remotelink-app: {msg}");
                if let Ok(mut g) = self.last_error.lock() {
                    *g = Some(msg);
                }
                self.child = None;
                false
            }
            Err(e) => {
                let msg = format!("host process poll error: {e}");
                if let Ok(mut g) = self.last_error.lock() {
                    *g = Some(msg);
                }
                false
            }
        }
    }

    /// Last host error, if any.
    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|mut g| g.take())
    }

    /// Request process stop (kill). Best-effort.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap quickly so we don't leave zombies.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if std::time::Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                }
            }
            eprintln!("remotelink-app: host process stopped");
        }
    }
}

impl Drop for HostWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Locate `remotelink-host` next to the app, via env, or under target/.
fn resolve_host_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("REMOTELINK_HOST_EXE") {
        let path = PathBuf::from(p.trim());
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "REMOTELINK_HOST_EXE not found: {}",
            path.display()
        ));
    }

    let exe_name = if cfg!(windows) {
        "remotelink-host.exe"
    } else {
        "remotelink-host"
    };

    // Same directory as remotelink-app (portable package / install layout).
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            // Dev: target/debug/remotelink-app.exe → sibling host.
            // Also try ../release when running debug, and vice versa.
            if let Some(profile_dir) = dir.file_name() {
                if let Some(target_dir) = dir.parent() {
                    for alt in ["debug", "release"] {
                        if profile_dir != alt {
                            let candidate = target_dir.join(alt).join(exe_name);
                            if candidate.is_file() {
                                return Ok(candidate);
                            }
                        }
                    }
                }
            }
        }
    }

    // CARGO_MANIFEST_DIR is apps/desktop at compile time.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
        for profile in ["debug", "release"] {
            let candidate = root.join("target").join(profile).join(exe_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "could not find {exe_name}; build with `cargo build -p remotelink-host` \
         or place it next to remotelink-app / set REMOTELINK_HOST_EXE"
    ))
}
