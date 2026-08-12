//! Background live viewer session for the product shell.

use remotelink_net::TransportMode;
use remotelink_viewer::{LiveViewerHandle, LiveViewerSnapshot, RawInput, WsViewerConfig};

/// Live remote-desktop session (stays open until Disconnect).
pub struct ViewerWorker {
    handle: LiveViewerHandle,
}

impl ViewerWorker {
    /// Start a live WSS connect in the background.
    pub fn start(
        server: String,
        host_public_id: String,
        otp: String,
        transport: TransportMode,
        unattended: bool,
    ) -> Self {
        let cfg = WsViewerConfig {
            server,
            host_public_id,
            otp,
            transport,
            media_timeout: std::time::Duration::from_secs(45),
            unattended,
        };
        Self {
            handle: LiveViewerHandle::start(cfg),
        }
    }

    /// Latest frame + stats for the UI.
    pub fn snapshot(&self) -> LiveViewerSnapshot {
        self.handle.snapshot()
    }

    /// Forward a mouse/key sample to the host.
    pub fn send_input(&self, raw: RawInput) {
        self.handle.send_input(raw);
    }

    /// Hang up.
    pub fn request_stop(&self) {
        self.handle.request_stop();
    }

    /// Whether the background thread has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}
