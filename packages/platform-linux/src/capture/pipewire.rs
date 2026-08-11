//! Native PipeWire / portal screen-capture skeleton.
//!
//! # Production sequence (not yet linked)
//!
//! ```text
//! 1. xdg-desktop-portal Screencast CreateSession / SelectSources / Start
//! 2. Receive PipeWire node id + optional fd from portal response
//! 3. pw_init / pw_context_connect / pw_stream_new for video
//! 4. Negotiate SPA_VIDEO_format (BGRx/RGBx preferred) + size/framerate
//! 5. On process: map buffer → VideoFrame { pts_host_mono, RGB/RGBA, data }
//! 6. On portal revoke / pw disconnect → CaptureError::SessionLost; agent reopens
//! ```
//!
//! # This build
//!
//! `libpipewire` and portal D-Bus bindings are intentionally **not** linked
//! (cross-compile CI, Windows GNU host builds). [`NativePipeWireCapture::try_open`]
//! always returns [`CaptureError::Unsupported`] or a device error describing
//! the missing link. Prefer [`super::open_capture`] with [`super::CaptureBackend::Mock`].

use std::time::Duration;

use remotelink_media::source::{VideoFrame, VideoSource as MediaVideoSource};

use super::source::{CaptureConfig, CaptureError};

/// Placeholder native PipeWire capture handle.
///
/// Once linked, this holds the portal session + `pw_stream` and implements
/// pull-based frame delivery with host-mono PTS.
#[derive(Debug)]
pub struct NativePipeWireCapture {
    display_index: u32,
    running: bool,
    start_pts: Duration,
}

impl NativePipeWireCapture {
    /// Whether the PipeWire-linked path is available in this build.
    pub fn is_available() -> bool {
        // Flip to true when libpipewire + portal session open is implemented.
        false
    }

    /// Attempt to open a PipeWire screencast stream for `config.display_index`.
    ///
    /// Currently always fails with a structured error so agents can fall back
    /// to mock / synthetic capture without panicking.
    pub fn try_open(config: CaptureConfig) -> Result<Self, CaptureError> {
        let _ = config;
        if !cfg!(target_os = "linux") {
            return Err(CaptureError::Unsupported);
        }
        // Linux target but not yet linked.
        Err(CaptureError::Device(
            "libpipewire / xdg-desktop-portal screencast not linked in this build; use CaptureBackend::Mock"
                .into(),
        ))
    }

    /// Construct a stopped skeleton for type-level tests (no frames).
    pub fn skeleton_for_tests(config: CaptureConfig) -> Self {
        Self {
            display_index: config.display_index,
            running: false,
            start_pts: Duration::from_millis(config.start_pts_ms),
        }
    }

    /// Backend name for logs.
    pub fn backend_name(&self) -> &'static str {
        "pipewire"
    }

    /// Configured display index.
    pub fn display_index(&self) -> u32 {
        self.display_index
    }

    /// Whether capture would be running (always false until linked).
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Host-mono PTS origin reserved for the open client.
    pub fn start_pts(&self) -> Duration {
        self.start_pts
    }

    /// Stop capture (no-op skeleton).
    pub fn stop(&mut self) {
        self.running = false;
    }
}

impl MediaVideoSource for NativePipeWireCapture {
    type Error = CaptureError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        // Unreachable until try_open succeeds and sets running.
        Err(CaptureError::Device("pipewire stream not linked".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_open_fails_structured() {
        let err = NativePipeWireCapture::try_open(CaptureConfig::default()).unwrap_err();
        #[cfg(target_os = "linux")]
        {
            assert!(matches!(err, CaptureError::Device(_)));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(matches!(err, CaptureError::Unsupported));
        }
    }

    #[test]
    fn not_available_until_linked() {
        assert!(!NativePipeWireCapture::is_available());
    }

    #[test]
    fn skeleton_yields_none_when_stopped() {
        let mut cap = NativePipeWireCapture::skeleton_for_tests(CaptureConfig::default());
        assert_eq!(cap.backend_name(), "pipewire");
        assert!(!cap.is_running());
        assert!(cap.next_frame().unwrap().is_none());
    }
}
