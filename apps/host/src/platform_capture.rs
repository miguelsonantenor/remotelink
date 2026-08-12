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
    /// Media synthetic color bars (default on non-Windows/Linux; tests everywhere).
    #[default]
    Synthetic,
    /// Platform-linux mock frames (deterministic, no compositor).
    LinuxMock,
    /// Platform-linux PipeWire platform backend (errors until native is linked).
    LinuxPlatform,
    /// Windows DXGI mock frames (CI-safe desktop-shaped BGRA).
    WindowsMock,
    /// Windows DXGI Desktop Duplication (interactive session; may fail headless).
    WindowsDxgi,
}

/// Which audio path the agent should use when starting media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCaptureKind {
    /// Media synthetic A440 tone (default on non-Windows/Linux; tests everywhere).
    #[default]
    Synthetic,
    /// Platform-linux mock monitor (PCM tone, no PipeWire).
    LinuxMockMonitor,
    /// Prefer native monitor with mock fallback (`PreferNative`).
    LinuxPreferNative,
    /// Require native monitor (structured error until linked).
    LinuxNativeOnly,
    /// Windows WASAPI loopback stub (CI-safe synthetic system audio).
    WindowsWasapiStub,
    /// Prefer native WASAPI loopback; fall back to stub when COM is unavailable.
    WindowsWasapiPreferNative,
    /// Require native WASAPI (errors until COM loopback is linked).
    WindowsWasapiNativeOnly,
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
    /// Windows DXGI / mock capture open failed.
    #[error("windows video capture: {0}")]
    WindowsVideo(#[from] remotelink_platform_windows::CaptureError),
    /// Windows WASAPI loopback open failed.
    #[error("windows audio loopback: {0}")]
    WindowsAudio(#[from] remotelink_platform_windows::LoopbackError),
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
    /// Windows DXGI Desktop Duplication or mock BGRA frames.
    Windows(remotelink_platform_windows::DisplayCapture),
}

impl fmt::Debug for HostVideoSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic(_) => f.write_str("HostVideoSource::Synthetic(..)"),
            Self::Linux(c) => write!(f, "HostVideoSource::Linux({})", c.backend_name()),
            Self::Windows(_) => f.write_str("HostVideoSource::Windows(..)"),
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
            Self::Windows(c) => {
                use remotelink_platform_windows::CaptureVideoSource;
                match c.next_frame() {
                    Ok(None) => Ok(None),
                    Ok(Some(f)) => Ok(Some(windows_frame_to_media(f)?)),
                    Err(e) => Err(PlatformCaptureError::from(e)),
                }
            }
        }
    }
}

impl HostVideoSource {
    /// Backend name for logs / stats.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Synthetic(_) => "synthetic",
            Self::Linux(c) => c.backend_name(),
            Self::Windows(remotelink_platform_windows::DisplayCapture::Mock(_)) => "windows-mock",
            #[cfg(windows)]
            Self::Windows(remotelink_platform_windows::DisplayCapture::Dxgi(_)) => "dxgi",
            #[cfg(not(windows))]
            Self::Windows(_) => "windows",
        }
    }
}

/// Convert a platform-windows capture frame into a tightly packed media frame.
fn windows_frame_to_media(
    f: remotelink_platform_windows::CaptureVideoFrame,
) -> Result<VideoFrame, PlatformCaptureError> {
    if !f.is_well_formed() {
        return Err(PlatformCaptureError::Unavailable(
            "windows capture frame not well-formed",
        ));
    }
    let format = match f.format {
        remotelink_platform_windows::CapturePixelFormat::Bgra8 => {
            remotelink_media::PixelFormat::Bgra8
        }
        remotelink_platform_windows::CapturePixelFormat::Rgba8 => {
            remotelink_media::PixelFormat::Rgba8
        }
        remotelink_platform_windows::CapturePixelFormat::Rgb24 => {
            remotelink_media::PixelFormat::Rgb24
        }
    };
    let bpp = format.bytes_per_pixel();
    let row_bytes = (f.width as usize).saturating_mul(bpp);
    let stride = f.stride as usize;
    let mut data = Vec::with_capacity(row_bytes.saturating_mul(f.height as usize));
    for y in 0..f.height as usize {
        let start = y.saturating_mul(stride);
        let end = start.saturating_add(row_bytes);
        if end > f.data.len() {
            return Err(PlatformCaptureError::Unavailable(
                "windows capture stride overflow",
            ));
        }
        data.extend_from_slice(&f.data[start..end]);
    }
    Ok(VideoFrame {
        pts_host_mono: f.pts_host_mono,
        width: f.width,
        height: f.height,
        format,
        data,
    })
}

/// Opened audio source for the agent media plane.
pub enum HostAudioSource {
    /// Media crate synthetic tone.
    Synthetic(SyntheticAudioTone),
    /// Linux platform monitor (mock or native handle).
    Linux(remotelink_platform_linux::AnyMonitor),
    /// Windows WASAPI loopback (stub or native skeleton).
    Windows(remotelink_platform_windows::AnyLoopback),
}

impl fmt::Debug for HostAudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic(_) => f.write_str("HostAudioSource::Synthetic(..)"),
            Self::Linux(m) => write!(f, "HostAudioSource::Linux({})", m.backend_name()),
            Self::Windows(m) => {
                use remotelink_platform_windows::LoopbackSource;
                write!(f, "HostAudioSource::Windows({})", m.backend_name())
            }
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
            Self::Windows(m) => m.next_frame().map_err(PlatformCaptureError::from),
        }
    }
}

impl HostAudioSource {
    /// Backend name for logs / stats.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Synthetic(_) => "synthetic",
            Self::Linux(m) => m.backend_name(),
            Self::Windows(m) => {
                use remotelink_platform_windows::LoopbackSource;
                m.backend_name()
            }
        }
    }
}

/// Default video kind for this compile target.
///
/// - Linux → mock PipeWire path
/// - Windows → DXGI mock (desktop-shaped BGRA; real DXGI via [`VideoCaptureKind::WindowsDxgi`])
/// - Else → media synthetic bars
pub fn default_video_kind() -> VideoCaptureKind {
    if cfg!(target_os = "linux") {
        VideoCaptureKind::LinuxMock
    } else if cfg!(windows) {
        VideoCaptureKind::WindowsMock
    } else {
        VideoCaptureKind::Synthetic
    }
}

/// Default audio kind for this compile target.
///
/// - Linux → prefer native monitor (mock fallback)
/// - Windows → WASAPI stub loopback (native COM not linked yet; PreferNative falls back)
/// - Else → media synthetic tone
pub fn default_audio_kind() -> AudioCaptureKind {
    if cfg!(target_os = "linux") {
        AudioCaptureKind::LinuxPreferNative
    } else if cfg!(windows) {
        AudioCaptureKind::WindowsWasapiStub
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
        VideoCaptureKind::WindowsMock => {
            let cfg = remotelink_platform_windows::CaptureConfig {
                display_index: 0,
                timeout_ms: 16,
                mock_width: width.max(1),
                mock_height: height.max(1),
                mock_fps: fps.max(1),
                mock_start_pts_ms: duration_to_ms(start_pts),
            };
            let cap = remotelink_platform_windows::open_capture(
                remotelink_platform_windows::CaptureBackend::Mock,
                cfg,
            )?;
            Ok(HostVideoSource::Windows(cap))
        }
        VideoCaptureKind::WindowsDxgi => {
            let cfg = remotelink_platform_windows::CaptureConfig {
                display_index: 0,
                timeout_ms: 16,
                ..remotelink_platform_windows::CaptureConfig::default()
            };
            let cap = remotelink_platform_windows::open_capture(
                remotelink_platform_windows::CaptureBackend::Platform,
                cfg,
            )?;
            Ok(HostVideoSource::Windows(cap))
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
        AudioCaptureKind::WindowsWasapiStub => {
            let cfg = remotelink_platform_windows::LoopbackConfig {
                open_mode: remotelink_platform_windows::LoopbackOpenMode::StubOnly,
                start_pts_ms: duration_to_ms(start_pts),
                channels: 1,
                ..remotelink_platform_windows::LoopbackConfig::default()
            };
            let m = remotelink_platform_windows::open_loopback(cfg)?;
            Ok(HostAudioSource::Windows(m))
        }
        AudioCaptureKind::WindowsWasapiPreferNative => {
            let cfg = remotelink_platform_windows::LoopbackConfig {
                open_mode: remotelink_platform_windows::LoopbackOpenMode::PreferNative,
                start_pts_ms: duration_to_ms(start_pts),
                ..remotelink_platform_windows::LoopbackConfig::default()
            };
            let m = remotelink_platform_windows::open_loopback(cfg)?;
            Ok(HostAudioSource::Windows(m))
        }
        AudioCaptureKind::WindowsWasapiNativeOnly => {
            let cfg = remotelink_platform_windows::LoopbackConfig {
                open_mode: remotelink_platform_windows::LoopbackOpenMode::NativeOnly,
                start_pts_ms: duration_to_ms(start_pts),
                ..remotelink_platform_windows::LoopbackConfig::default()
            };
            let m = remotelink_platform_windows::open_loopback(cfg)?;
            Ok(HostAudioSource::Windows(m))
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
        } else if cfg!(windows) {
            assert_eq!(default_video_kind(), VideoCaptureKind::WindowsMock);
            assert_eq!(default_audio_kind(), AudioCaptureKind::WindowsWasapiStub);
        } else {
            assert_eq!(default_video_kind(), VideoCaptureKind::Synthetic);
            assert_eq!(default_audio_kind(), AudioCaptureKind::Synthetic);
        }
    }

    #[test]
    fn windows_wasapi_stub_audio_works() {
        let mut a = open_audio_source(
            AudioCaptureKind::WindowsWasapiStub,
            Duration::from_millis(5),
        )
        .unwrap();
        assert_eq!(a.backend_name(), "stub");
        let f = a.next_frame().unwrap().unwrap();
        assert_eq!(f.pts_host_mono, Duration::from_millis(5));
        assert!(f.frame_count() > 0);
    }

    #[test]
    fn windows_wasapi_prefer_native_opens_or_stub() {
        let mut a =
            open_audio_source(AudioCaptureKind::WindowsWasapiPreferNative, Duration::ZERO).unwrap();
        // Real COM when a render endpoint exists; otherwise stub fallback.
        let name = a.backend_name();
        assert!(name == "wasapi" || name == "stub", "backend={name}");
        // Stub always yields frames; native may be idle (None) if no audio playing.
        let _ = a.next_frame();
    }

    #[test]
    fn windows_wasapi_native_only_open_or_error() {
        match open_audio_source(AudioCaptureKind::WindowsWasapiNativeOnly, Duration::ZERO) {
            Ok(a) => assert_eq!(a.backend_name(), "wasapi"),
            Err(PlatformCaptureError::WindowsAudio(_)) => {}
            Err(e) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn windows_mock_video_works() {
        let mut v = open_video_source(
            VideoCaptureKind::WindowsMock,
            64,
            36,
            30,
            Duration::from_millis(5),
        )
        .unwrap();
        assert_eq!(v.backend_name(), "windows-mock");
        let f = v.next_frame().unwrap().unwrap();
        assert_eq!(f.width, 64);
        assert_eq!(f.height, 36);
        assert_eq!(f.format, remotelink_media::PixelFormat::Bgra8);
        assert!(f.is_well_formed());
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
