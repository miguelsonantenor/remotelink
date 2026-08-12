//! Desktop video capture for the session agent (KD5 agent-media).
//!
//! Production path on Windows is **DXGI Desktop Duplication**, producing
//! BGRA frames stamped with host-monotonic capture time (`host_mono`) for the
//! A/V timing contract (DESIGN). Encode and PeerTransport stay in-process in
//! the agent (PR 16b+); this module only delivers raw frames to a [`FrameSink`].
//!
//! # Secure desktop / UAC (v1 known gap)
//!
//! Capture **does not work** on the Winlogon / UAC secure desktop without a
//! separate signed path (credential provider / special driver) — **out of scope
//! for v1**. When the host enters a secure desktop, Desktop Duplication returns
//! access-lost ([`CaptureError::AccessLost`]); remote users cannot see or
//! interact with UAC prompts or Ctrl+Alt+Del screens. The host user must
//! complete those locally. The tray kill-switch remains available on the normal
//! desktop only.
//!
//! # Testing without a real desktop
//!
//! Use [`CaptureBackend::Mock`] / [`MockVideoSource`]. Unit tests must not
//! require an interactive session or GPU.
//!
//! # Non-Windows
//!
//! [`open_capture`] with [`CaptureBackend::Platform`] returns
//! [`CaptureError::Unsupported`]. Mock backend works on all platforms.

mod frame;
mod mock;
mod source;

#[cfg(windows)]
mod dxgi;

pub use frame::{host_mono_now, PixelFormat, VideoFrame};
pub use mock::MockVideoSource;
pub use source::{
    pump_frame, CaptureBackend, CaptureConfig, CaptureError, CollectingFrameSink, FrameSink,
    PumpError, VideoSource,
};

/// Opened capture handle implementing [`VideoSource`].
#[derive(Debug)]
pub enum DisplayCapture {
    /// Deterministic mock frames (tests / CI).
    Mock(MockVideoSource),
    /// DXGI Desktop Duplication (Windows interactive session).
    #[cfg(windows)]
    Dxgi(dxgi::DxgiDesktopDuplication),
}

impl VideoSource for DisplayCapture {
    type Error = CaptureError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        match self {
            DisplayCapture::Mock(m) => m.next_frame(),
            #[cfg(windows)]
            DisplayCapture::Dxgi(d) => d.next_frame(),
        }
    }
}

/// Open a display capturer for the given backend and config.
///
/// - [`CaptureBackend::Mock`]: always succeeds (no GPU / desktop).
/// - [`CaptureBackend::Platform`]: DXGI on Windows; [`CaptureError::Unsupported`] elsewhere.
pub fn open_capture(
    backend: CaptureBackend,
    config: CaptureConfig,
) -> Result<DisplayCapture, CaptureError> {
    match backend {
        CaptureBackend::Mock => {
            // Prefer explicit mock geometry when set; otherwise 320×180 @ ~30 fps.
            let w = if config.mock_width > 0 {
                config.mock_width
            } else {
                320
            };
            let h = if config.mock_height > 0 {
                config.mock_height
            } else {
                180
            };
            let interval_ms = if config.mock_fps > 0 {
                1000u32 / config.mock_fps.max(1)
            } else {
                33
            };
            let mut src = MockVideoSource::new(
                w,
                h,
                PixelFormat::Bgra8,
                std::time::Duration::from_millis(u64::from(interval_ms.max(1))),
            );
            if config.mock_start_pts_ms > 0 {
                src =
                    src.with_start_pts(std::time::Duration::from_millis(config.mock_start_pts_ms));
            }
            Ok(DisplayCapture::Mock(src))
        }
        CaptureBackend::Platform => open_platform_capture(config),
    }
}

#[cfg(windows)]
fn open_platform_capture(config: CaptureConfig) -> Result<DisplayCapture, CaptureError> {
    let dup = dxgi::DxgiDesktopDuplication::open(&config)?;
    Ok(DisplayCapture::Dxgi(dup))
}

#[cfg(not(windows))]
fn open_platform_capture(_config: CaptureConfig) -> Result<DisplayCapture, CaptureError> {
    Err(CaptureError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn open_mock_backend_pumps_to_sink() {
        let mut cap = open_capture(CaptureBackend::Mock, CaptureConfig::default()).unwrap();
        // Finite stream: wrap isn't finite by default; pull a few frames then stop.
        let mut sink = CollectingFrameSink::new();
        for _ in 0..5 {
            assert!(pump_frame(&mut cap, &mut sink).unwrap());
        }
        assert_eq!(sink.len(), 5);
        for f in &sink.frames {
            assert!(f.is_well_formed());
            assert_eq!(f.format, PixelFormat::Bgra8);
            assert_eq!(f.width, 320);
            assert_eq!(f.height, 180);
        }
    }

    #[test]
    fn platform_backend_on_non_desktop_is_error_or_handle() {
        // On non-Windows this must be Unsupported. On Windows with a desktop it may
        // succeed; headless agents may get Device/AccessLost — never panic.
        let result = open_capture(CaptureBackend::Platform, CaptureConfig::default());
        #[cfg(not(windows))]
        {
            assert!(matches!(result, Err(CaptureError::Unsupported)));
        }
        #[cfg(windows)]
        {
            match result {
                Ok(mut cap) => {
                    // Idle timeout → Ok(None) is fine; any error is also acceptable.
                    let _ = cap.next_frame();
                }
                Err(CaptureError::Unsupported)
                | Err(CaptureError::AccessLost)
                | Err(CaptureError::Device(_))
                | Err(CaptureError::DisplayNotFound(_)) => {}
                Err(e) => panic!("unexpected platform open error: {e}"),
            }
        }
    }

    #[test]
    fn capture_error_display_messages() {
        assert!(CaptureError::Unsupported
            .to_string()
            .contains("unsupported"));
        assert!(CaptureError::AccessLost.to_string().contains("access lost"));
        assert!(CaptureError::DisplayNotFound(3).to_string().contains('3'));
        let _ = Duration::from_millis(1);
    }
}
