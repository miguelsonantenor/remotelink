//! Host-side capture backend selection (Windows primary, Linux secondary).
//!
//! Used by [`crate::session::SessionManager::start_media`]: kinds default to
//! [`default_video_kind`] / [`default_audio_kind`] (Linux → platform-linux mock /
//! PreferNative; other OS → media synthetic). Callers may override via
//! [`SessionManager::set_video_kind`](crate::session::SessionManager::set_video_kind)
//! / `set_audio_kind` before starting media.
//!
//! KD5: capture stays in the agent process; this module constructs sources only.

use std::fmt;
use std::time::Duration;

use remotelink_media::{
    AudioFrame, AudioSource, SyntheticAudioTone, SyntheticVideoBars, VideoFrame, VideoSource,
};
use remotelink_platform_linux::MonitorSource;

/// Which video path the agent should use when starting media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCaptureKind {
    /// Media synthetic color bars (default on non-Linux; tests everywhere).
    #[default]
    Synthetic,
    /// Platform-linux mock frames (deterministic, no compositor).
    LinuxMock,
    /// Platform-linux PipeWire platform backend (errors until native is linked).
    LinuxPlatform,
}

/// Which audio path the agent should use when starting media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCaptureKind {
    /// Media synthetic A440 tone (default on non-Linux; tests everywhere).
    #[default]
    Synthetic,
    /// Platform-linux mock monitor (PCM tone, no PipeWire).
    LinuxMockMonitor,
    /// Prefer native monitor with mock fallback (`PreferNative`).
    LinuxPreferNative,
    /// Require native monitor (structured error until linked).
    LinuxNativeOnly,
}

/// Errors opening a host capture backend.
#[derive(Debug, thiserror::Error)]
pub enum PlatformCaptureError {
    /// Linux video capture open failed.
    #[error("linux video capture: {0}")]
    LinuxVideo(#[from] remotelink_platform_linux::CaptureError),
    /// Linux audio monitor open failed.
    #[error("linux audio monitor: {0}")]
    LinuxAudio(#[from] remotelink_platform_linux::MonitorError),
    /// Requested backend is not available on this OS / build.
    #[error("capture backend unavailable: {0}")]
    Unavailable(&'static str),
}

/// Opened video source for the agent media plane.
pub enum HostVideoSource {
    /// Media crate synthetic bars.
    Synthetic(SyntheticVideoBars),
    /// Linux platform display capture (mock or pipewire handle).
    Linux(remotelink_platform_linux::DisplayCapture),
}

impl fmt::Debug for HostVideoSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic(_) => f.write_str("HostVideoSource::Synthetic(..)"),
            Self::Linux(c) => write!(f, "HostVideoSource::Linux({})", c.backend_name()),
        }
    }
}

impl VideoSource for HostVideoSource {
    type Error = PlatformCaptureError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        match self {
            Self::Synthetic(s) => s
                .next_frame()
                .map_err(|_| PlatformCaptureError::Unavailable("synthetic video source error")),
            Self::Linux(c) => c.next_frame().map_err(PlatformCaptureError::from),
        }
    }
}

impl HostVideoSource {
    /// Backend name for logs / stats.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Synthetic(_) => "synthetic",
            Self::Linux(c) => c.backend_name(),
        }
    }
}

/// Opened audio source for the agent media plane.
pub enum HostAudioSource {
    /// Media crate synthetic tone.
    Synthetic(SyntheticAudioTone),
    /// Linux platform monitor (mock or native handle).
    Linux(remotelink_platform_linux::AnyMonitor),
}

impl fmt::Debug for HostAudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic(_) => f.write_str("HostAudioSource::Synthetic(..)"),
            Self::Linux(m) => write!(f, "HostAudioSource::Linux({})", m.backend_name()),
        }
    }
}

impl AudioSource for HostAudioSource {
    type Error = PlatformCaptureError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        match self {
            Self::Synthetic(s) => s
                .next_frame()
                .map_err(|_| PlatformCaptureError::Unavailable("synthetic audio source error")),
            Self::Linux(m) => m.next_frame().map_err(PlatformCaptureError::from),
        }
    }
}

impl HostAudioSource {
    /// Backend name for logs / stats.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Synthetic(_) => "synthetic",
            Self::Linux(m) => m.backend_name(),
        }
    }
}

/// Default video kind for this compile target.
///
/// Linux → mock PipeWire path (native returns error until linked); elsewhere synthetic.
pub fn default_video_kind() -> VideoCaptureKind {
    if cfg!(target_os = "linux") {
        VideoCaptureKind::LinuxMock
    } else {
        VideoCaptureKind::Synthetic
    }
}

/// Default audio kind for this compile target.
pub fn default_audio_kind() -> AudioCaptureKind {
    if cfg!(target_os = "linux") {
        AudioCaptureKind::LinuxPreferNative
    } else {
        AudioCaptureKind::Synthetic
    }
}

/// Open a video source for the given kind and geometry / epoch.
pub fn open_video_source(
    kind: VideoCaptureKind,
    width: u32,
    height: u32,
    fps: u32,
    start_pts: Duration,
) -> Result<HostVideoSource, PlatformCaptureError> {
    match kind {
        VideoCaptureKind::Synthetic => {
            let w = width.max(1);
            let h = height.max(1);
            let f = fps.max(1);
            Ok(HostVideoSource::Synthetic(SyntheticVideoBars::new(
                w, h, f, start_pts,
            )))
        }
        VideoCaptureKind::LinuxMock => {
            let cfg = remotelink_platform_linux::CaptureConfig {
                display_index: 0,
                width: width.max(1),
                height: height.max(1),
                frame_interval_ms: fps_to_interval_ms(fps),
                start_pts_ms: duration_to_ms(start_pts),
            };
            let cap = remotelink_platform_linux::open_capture(
                remotelink_platform_linux::CaptureBackend::Mock,
                cfg,
            )?;
            Ok(HostVideoSource::Linux(cap))
        }
        VideoCaptureKind::LinuxPlatform => {
            // Platform open returns Unsupported (non-Linux) or Device (Linux
            // until libpipewire is linked) — same structured error as the crate API.
            let cfg = remotelink_platform_linux::CaptureConfig {
                display_index: 0,
                width,
                height,
                frame_interval_ms: fps_to_interval_ms(fps),
                start_pts_ms: duration_to_ms(start_pts),
            };
            let cap = remotelink_platform_linux::open_capture(
                remotelink_platform_linux::CaptureBackend::Platform,
                cfg,
            )?;
            Ok(HostVideoSource::Linux(cap))
        }
    }
}

/// Open an audio source for the given kind and epoch.
pub fn open_audio_source(
    kind: AudioCaptureKind,
    start_pts: Duration,
) -> Result<HostAudioSource, PlatformCaptureError> {
    match kind {
        AudioCaptureKind::Synthetic => Ok(HostAudioSource::Synthetic(
            SyntheticAudioTone::default_a440(start_pts),
        )),
        AudioCaptureKind::LinuxMockMonitor => {
            let cfg = remotelink_platform_linux::MonitorConfig {
                open_mode: remotelink_platform_linux::MonitorOpenMode::StubOnly,
                start_pts_ms: duration_to_ms(start_pts),
                channels: 1,
                ..remotelink_platform_linux::MonitorConfig::default()
            };
            let m = remotelink_platform_linux::open_monitor(cfg)?;
            Ok(HostAudioSource::Linux(m))
        }
        AudioCaptureKind::LinuxPreferNative => {
            let cfg = remotelink_platform_linux::MonitorConfig {
                open_mode: remotelink_platform_linux::MonitorOpenMode::PreferNative,
                start_pts_ms: duration_to_ms(start_pts),
                ..remotelink_platform_linux::MonitorConfig::default()
            };
            let m = remotelink_platform_linux::open_monitor(cfg)?;
            Ok(HostAudioSource::Linux(m))
        }
        AudioCaptureKind::LinuxNativeOnly => {
            let cfg = remotelink_platform_linux::MonitorConfig {
                open_mode: remotelink_platform_linux::MonitorOpenMode::NativeOnly,
                start_pts_ms: duration_to_ms(start_pts),
                ..remotelink_platform_linux::MonitorConfig::default()
            };
            let m = remotelink_platform_linux::open_monitor(cfg)?;
            Ok(HostAudioSource::Linux(m))
        }
    }
}

/// Open the default backends for this target (Linux mock/prefer-native; else synthetic).
pub fn open_default_sources(
    width: u32,
    height: u32,
    fps: u32,
    start_pts: Duration,
) -> Result<(HostVideoSource, HostAudioSource), PlatformCaptureError> {
    let video = open_video_source(default_video_kind(), width, height, fps, start_pts)?;
    let audio = open_audio_source(default_audio_kind(), start_pts)?;
    Ok((video, audio))
}

fn duration_to_ms(d: Duration) -> u64 {
    d.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Convert fps to frame interval milliseconds (default 33 ms when fps is 0).
fn fps_to_interval_ms(fps: u32) -> u32 {
    1000u32.checked_div(fps).unwrap_or(33).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kinds_match_target() {
        if cfg!(target_os = "linux") {
            assert_eq!(default_video_kind(), VideoCaptureKind::LinuxMock);
            assert_eq!(default_audio_kind(), AudioCaptureKind::LinuxPreferNative);
        } else {
            assert_eq!(default_video_kind(), VideoCaptureKind::Synthetic);
            assert_eq!(default_audio_kind(), AudioCaptureKind::Synthetic);
        }
    }

    #[test]
    fn open_default_sources_works_on_this_target() {
        let (mut v, mut a) = open_default_sources(64, 36, 30, Duration::from_millis(0)).unwrap();
        let vf = v.next_frame().unwrap().unwrap();
        let af = a.next_frame().unwrap().unwrap();
        assert!(vf.is_well_formed());
        assert!(af.frame_count() > 0);
        assert!(!v.backend_name().is_empty());
        assert!(!a.backend_name().is_empty());
    }

    #[test]
    fn linux_mock_video_and_monitor_always_work() {
        let mut v = open_video_source(
            VideoCaptureKind::LinuxMock,
            32,
            18,
            15,
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(v.backend_name(), "mock");
        let f = v.next_frame().unwrap().unwrap();
        assert_eq!(f.width, 32);
        assert_eq!(f.height, 18);
        assert_eq!(f.pts_host_mono, Duration::from_millis(10));

        let mut a = open_audio_source(
            AudioCaptureKind::LinuxMockMonitor,
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(a.backend_name(), "mock");
        let af = a.next_frame().unwrap().unwrap();
        assert_eq!(af.pts_host_mono, Duration::from_millis(10));
        assert_eq!(af.duration(), Duration::from_millis(10));
    }

    #[test]
    fn linux_platform_video_errors_until_linked() {
        let err = open_video_source(VideoCaptureKind::LinuxPlatform, 0, 0, 30, Duration::ZERO)
            .unwrap_err();
        assert!(matches!(
            err,
            PlatformCaptureError::LinuxVideo(
                remotelink_platform_linux::CaptureError::Unsupported
                    | remotelink_platform_linux::CaptureError::Device(_)
            )
        ));
    }

    #[test]
    fn linux_native_only_audio_errors_until_linked() {
        let err = open_audio_source(AudioCaptureKind::LinuxNativeOnly, Duration::ZERO).unwrap_err();
        assert!(matches!(err, PlatformCaptureError::LinuxAudio(_)));
    }

    #[test]
    fn synthetic_path_still_works() {
        let mut v =
            open_video_source(VideoCaptureKind::Synthetic, 16, 9, 30, Duration::ZERO).unwrap();
        assert_eq!(v.backend_name(), "synthetic");
        assert!(v.next_frame().unwrap().unwrap().is_well_formed());

        let mut a = open_audio_source(AudioCaptureKind::Synthetic, Duration::ZERO).unwrap();
        assert_eq!(a.backend_name(), "synthetic");
        assert!(a.next_frame().unwrap().is_some());
    }
}
