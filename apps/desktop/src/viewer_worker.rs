//! Background viewer connect job (does not block the UI).

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use remotelink_net::TransportMode;
use remotelink_viewer::{run_ws_viewer_blocking, WsViewerConfig};

/// Result of a finished connect attempt.
#[derive(Debug, Clone)]
pub enum ConnectOutcome {
    /// Still connecting.
    Running,
    /// Success summary from the viewer path.
    Ok(String),
    /// Failure message.
    Err(String),
}

/// One-shot connect worker.
pub struct ViewerWorker {
    outcome: Arc<Mutex<ConnectOutcome>>,
    join: Option<JoinHandle<()>>,
}

impl ViewerWorker {
    /// Start a WSS connect in the background.
    pub fn start(
        server: String,
        host_public_id: String,
        otp: String,
        transport: TransportMode,
    ) -> Self {
        let outcome = Arc::new(Mutex::new(ConnectOutcome::Running));
        let slot = Arc::clone(&outcome);
        let join = thread::Builder::new()
            .name("remotelink-viewer".into())
            .spawn(move || {
                let cfg = WsViewerConfig {
                    server,
                    host_public_id,
                    otp,
                    transport,
                    media_timeout: std::time::Duration::from_secs(45),
                };
                let result = run_ws_viewer_blocking(cfg);
                if let Ok(mut g) = slot.lock() {
                    *g = match result {
                        Ok(s) => ConnectOutcome::Ok(s),
                        Err(e) => ConnectOutcome::Err(e),
                    };
                }
            })
            .expect("spawn viewer thread");

        Self {
            outcome,
            join: Some(join),
        }
    }

    /// Poll current outcome.
    pub fn poll(&self) -> ConnectOutcome {
        self.outcome
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| ConnectOutcome::Err("viewer lock poisoned".into()))
    }

    /// Whether the background thread has finished.
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
    }
}

impl Drop for ViewerWorker {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}
