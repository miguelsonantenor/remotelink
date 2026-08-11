//! Background host service for "this PC" (allow remote access).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use remotelink_host::{run_ws_host_blocking, WsHostConfig};
use remotelink_net::TransportMode;

/// Lifecycle handle for the in-process host service thread.
pub struct HostWorker {
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl HostWorker {
    /// Spawn the WSS host service on a dedicated thread.
    pub fn start(
        server: String,
        display_name: String,
        transport: TransportMode,
        status_path: PathBuf,
        creds_path: PathBuf,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(std::sync::Mutex::new(None));
        let running_flag = Arc::clone(&running);
        let err_slot = Arc::clone(&last_error);

        let join = thread::Builder::new()
            .name("remotelink-host".into())
            .spawn(move || {
                let cfg = WsHostConfig {
                    server,
                    display_name,
                    transport,
                    video_frames: 5,
                    wait_incoming: std::time::Duration::from_secs(300),
                    existing: None,
                    max_sessions: 0, // unlimited
                    reconnect: true,
                    reconnect_backoff: std::time::Duration::from_secs(2),
                    creds_path,
                    load_creds: true,
                    save_creds: true,
                    mint_otp: true,
                    agent_control: None,
                    tray: true,       // status file + console
                    os_tray: false,   // product UI owns the surface
                    status_path: Some(status_path),
                    boot_secret: None,
                };
                running_flag.store(true, Ordering::SeqCst);
                match run_ws_host_blocking(cfg) {
                    Ok(summary) => {
                        eprintln!("remotelink-app: host stopped: {summary}");
                    }
                    Err(e) => {
                        eprintln!("remotelink-app: host error: {e}");
                        if let Ok(mut g) = err_slot.lock() {
                            *g = Some(e);
                        }
                    }
                }
                running_flag.store(false, Ordering::SeqCst);
            })
            .expect("spawn host thread");

        Self {
            running,
            join: Some(join),
            last_error,
        }
    }

    /// Whether the host thread is still marked running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
            && self
                .join
                .as_ref()
                .map(|j| !j.is_finished())
                .unwrap_or(false)
    }

    /// Last host error, if any.
    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for HostWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Host service loop does not yet support cooperative stop; process exit
        // tears the thread down. Detach to avoid blocking the UI on drop.
        if let Some(join) = self.join.take() {
            // If already finished, join to surface panics; else leave it.
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}
