//! Desktop video capture for the Linux session agent (KD5 agent-media).
//!
//! Production path is **PipeWire** via xdg-desktop-portal Screencast, producing
//! frames stamped with host-monotonic capture time for the A/V timing contract.
//! Encode and PeerTransport stay in-process in the agent; this module only
//! delivers raw frames (or mock frames in CI).
//!
//! # Portal / Wayland notes (v1 known gaps)
//!
//! - User must grant portal permission per session (or restore token when available).
//! - Secure-attention / lock screens may not be capturable depending on compositor.
//! - Multi-monitor: v1 uses a single selected stream (`display_index`).
//!
//! # Testing without a compositor
//!
//! Use [`CaptureBackend::Mock`] / [`MockVideoSource`]. Unit tests must not
//! require PipeWire, a session bus, or a GPU.
//!
//! # Non-Linux
//!
//! [`open_capture`] with [`CaptureBackend::Platform`] returns
//! [`CaptureError::Unsupported`]. Mock backend works on all platforms.

mod mock;
mod pipewire;
mod source;

use std::time::Duration;

pub use mock::MockVideoSource;
pub use pipewire::NativePipeWireCapture;
pub use source::{
    pump_frame, CaptureBackend, CaptureConfig, CaptureError, CollectingFrameSink, FrameSink,
    PumpError,
};

use remotelink_media::source::{PixelFormat, VideoSource};

/// Opened capture handle implementing media [`VideoSource`].
#[derive(Debug)]
pub enum DisplayCapture {
    /// Deterministic mock frames (tests / CI).
    Mock(MockVideoSource),
    /// PipeWire portal capture (Linux when linked).
    PipeWire(NativePipeWireCapture),
}

impl VideoSource for DisplayCapture {
    type Error = CaptureError;

    fn next_frame(&mut self) -> Result<Option<remotelink_media::VideoFrame>, Self::Error> {
        match self {
            DisplayCapture::Mock(m) => VideoSource::next_frame(m),
            DisplayCapture::PipeWire(p) => VideoSource::next_frame(p),
        }
    }
}

impl DisplayCapture {
    /// Backend name for logs (`"mock"` / `"pipewire"`).
    pub fn backend_name(&self) -> &'static str {
        match self {
            DisplayCapture::Mock(_) => "mock",
            DisplayCapture::PipeWire(p) => p.backend_name(),
        }
    }
}

/// Open a display capturer for the given backend and config.
///
/// - [`CaptureBackend::Mock`]: always succeeds (no compositor / PipeWire).
/// - [`CaptureBackend::Platform`]: PipeWire on Linux when available; else
///   [`CaptureError::Unsupported`] or [`CaptureError::Device`].
pub fn open_capture(
    backend: CaptureBackend,
    config: CaptureConfig,
) -> Result<DisplayCapture, CaptureError> {
    match backend {
        CaptureBackend::Mock => {
            let width = if config.width == 0 { 320 } else { config.width };
            let height = if config.height == 0 {
                180
            } else {
                config.height
            };
            let interval = Duration::from_millis(u64::from(config.frame_interval_ms.max(1)));
            let src = MockVideoSource::new(width, height, PixelFormat::Rgb24, interval)
                .with_start_pts(Duration::from_millis(config.start_pts_ms));
            let _ = config.display_index;
            Ok(DisplayCapture::Mock(src))
        }
        CaptureBackend::Platform => open_platform_capture(config),
    }
}

fn open_platform_capture(config: CaptureConfig) -> Result<DisplayCapture, CaptureError> {
    let cap = NativePipeWireCapture::try_open(config)?;
    Ok(DisplayCapture::PipeWire(cap))
}

/// Host-monotonic clock used for capture timestamps (`host_mono`).
///
/// Process-relative `Instant` — suitable for tests and for mapping to RTP via
/// a shared session epoch `t0` (DESIGN A/V timing contract). Linux production
/// may later prefer `CLOCK_MONOTONIC` via libc for tighter alignment with
/// PipeWire driver clocks.
pub fn host_mono_now() -> Duration {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_mock_backend_pumps_to_sink() {
        let mut cap = open_capture(CaptureBackend::Mock, CaptureConfig::default()).unwrap();
        assert_eq!(cap.backend_name(), "mock");
        let mut sink = CollectingFrameSink::new();
        for _ in 0..5 {
            assert!(pump_frame(&mut cap, &mut sink).unwrap());
        }
        assert_eq!(sink.len(), 5);
        for f in &sink.frames {
            assert!(f.is_well_formed());
            assert_eq!(f.format, PixelFormat::Rgb24);
            assert_eq!(f.width, 320);
            assert_eq!(f.height, 180);
        }
    }

    #[test]
    fn platform_backend_errors_until_linked() {
        let result = open_capture(CaptureBackend::Platform, CaptureConfig::default());
        assert!(result.is_err());
        let err = result.unwrap_err();
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
    fn host_mono_is_monotonic() {
        let a = host_mono_now();
        let b = host_mono_now();
        assert!(b >= a);
    }

    #[test]
    fn synthetic_config_defaults() {
        let c = CaptureConfig::synthetic();
        assert_eq!(c.display_index, 0);
        assert!(c.frame_interval_ms > 0);
    }
}
