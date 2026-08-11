//! Loopback capture configuration and open helpers.

use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};
use thiserror::Error;

use super::hooks::{LoopbackHooks, NullHooks};
use super::native::NativeLoopbackCapture;
use super::stub::StubLoopbackCapture;

/// RemoteLink default capture sample rate (Hz).
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// RemoteLink default channel count (stereo loopback downmix later if needed).
pub const DEFAULT_CHANNELS: u16 = 2;
/// Opus packet duration used by the host path (ms).
pub const DEFAULT_PACKET_MS: u32 = 10;

/// How [`open_loopback`] should select a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopbackOpenMode {
    /// Prefer native WASAPI; fall back to stub if native is unavailable.
    #[default]
    PreferNative,
    /// Always use the CI/synthetic stub (no COM).
    StubOnly,
    /// Require native WASAPI (error if unavailable).
    NativeOnly,
}

/// Configuration for opening a loopback capture client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackConfig {
    /// Sample rate in Hz (default 48_000).
    pub sample_rate: u32,
    /// Channel count (1 or 2).
    pub channels: u16,
    /// Packetization interval in milliseconds (typically 10).
    pub packet_ms: u32,
    /// Backend selection policy.
    pub open_mode: LoopbackOpenMode,
    /// Host-mono PTS origin for the first sample (media epoch `t0` candidate).
    pub start_pts_ms: u64,
    /// Near-zero energy sustained for this many ms → exclusive-mode warning.
    pub exclusive_silence_ms: u64,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            packet_ms: DEFAULT_PACKET_MS,
            open_mode: LoopbackOpenMode::PreferNative,
            start_pts_ms: 0,
            exclusive_silence_ms: 500,
        }
    }
}

impl LoopbackConfig {
    /// CI / unit-test defaults: stub backend, mono A440-friendly layout.
    pub fn synthetic() -> Self {
        Self {
            open_mode: LoopbackOpenMode::StubOnly,
            channels: 1,
            ..Self::default()
        }
    }
}

/// Errors from opening or reading loopback capture.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LoopbackError {
    /// Invalid configuration values.
    #[error("invalid loopback config: {0}")]
    InvalidConfig(&'static str),
    /// Native WASAPI path is not linked / not available on this build.
    #[error("native WASAPI loopback unavailable: {0}")]
    NativeUnavailable(&'static str),
    /// Opening the audio client failed (exclusive mode, missing device, …).
    #[error("loopback client open failed: {0}")]
    ClientOpenFailed(String),
    /// Capture read failed after open.
    #[error("loopback capture failed: {0}")]
    Capture(String),
    /// Device-change requires the agent to re-open the capture client.
    ///
    /// Native skeleton cannot hot-reopen; stub reanchors in place and does
    /// not return this error.
    #[error("loopback reopen required after device change")]
    ReopenRequired,
}

/// Type-erased loopback source implementing [`AudioSource`].
///
/// Both stub and (future) native backends expose the same pull API so the
/// session agent can feed PCM into [`remotelink_media::OpusEncoder`].
pub trait LoopbackSource: AudioSource<Error = LoopbackError> {
    /// Configured sample rate.
    fn sample_rate(&self) -> u32;
    /// Configured channel count.
    fn channels(&self) -> u16;
    /// Backend name for logs (`"stub"` / `"wasapi"`).
    fn backend_name(&self) -> &'static str;
    /// Stop capture; further `next_frame` returns `Ok(None)`.
    fn stop(&mut self);
    /// Whether capture is still running.
    fn is_running(&self) -> bool;
    /// Handle a device-change: reopen / re-anchor PTS to `new_start_pts`.
    ///
    /// Stub reanchors in place. Native skeleton returns
    /// [`LoopbackError::ReopenRequired`] so the agent re-opens via
    /// [`open_loopback`].
    fn inject_device_change(
        &mut self,
        reason: super::hooks::DeviceChangeReason,
        new_start_pts: Duration,
    ) -> Result<(), LoopbackError>;
    /// Force N silent packets (exclusive-mode detector tests).
    fn inject_silence_packets(&mut self, count: u32);
}

/// Concrete backend used by the agent media plane.
pub enum AnyLoopback {
    /// Software stub (always available).
    Stub(StubLoopbackCapture),
    /// Native WASAPI skeleton (open may fail until COM is implemented).
    Native(NativeLoopbackCapture),
}

impl std::fmt::Debug for AnyLoopback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stub(_) => f.write_str("AnyLoopback::Stub(..)"),
            Self::Native(_) => f.write_str("AnyLoopback::Native(..)"),
        }
    }
}

impl AudioSource for AnyLoopback {
    type Error = LoopbackError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        match self {
            Self::Stub(s) => s.next_frame(),
            Self::Native(n) => n.next_frame(),
        }
    }
}

impl LoopbackSource for AnyLoopback {
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Stub(s) => s.sample_rate(),
            Self::Native(n) => n.sample_rate(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            Self::Stub(s) => s.channels(),
            Self::Native(n) => n.channels(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            Self::Stub(s) => s.backend_name(),
            Self::Native(n) => n.backend_name(),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Stub(s) => s.stop(),
            Self::Native(n) => n.stop(),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            Self::Stub(s) => s.is_running(),
            Self::Native(n) => n.is_running(),
        }
    }

    fn inject_device_change(
        &mut self,
        reason: super::hooks::DeviceChangeReason,
        new_start_pts: Duration,
    ) -> Result<(), LoopbackError> {
        match self {
            Self::Stub(s) => s.inject_device_change(reason, new_start_pts),
            Self::Native(n) => n.inject_device_change(reason, new_start_pts),
        }
    }

    fn inject_silence_packets(&mut self, count: u32) {
        match self {
            Self::Stub(s) => s.inject_silence_packets(count),
            Self::Native(n) => n.inject_silence_packets(count),
        }
    }
}

/// Open a loopback source according to `config.open_mode`.
///
/// Uses [`NullHooks`] — prefer [`open_loopback_with_hooks`] when the agent
/// needs device-change / exclusive-mode callbacks.
pub fn open_loopback(config: LoopbackConfig) -> Result<AnyLoopback, LoopbackError> {
    open_loopback_with_hooks(config, Box::new(NullHooks))
}

/// Open a loopback source with event hooks.
///
/// PreferNative policy (single open, no probe):
/// 1. If native is not linked → stub.
/// 2. Else try native once with caller hooks; on `ClientOpenFailed` (or other
///    open errors) fire the hook and fall back to stub so the session continues.
pub fn open_loopback_with_hooks(
    config: LoopbackConfig,
    hooks: Box<dyn LoopbackHooks>,
) -> Result<AnyLoopback, LoopbackError> {
    validate_config(&config)?;
    match config.open_mode {
        LoopbackOpenMode::StubOnly => {
            Ok(AnyLoopback::Stub(StubLoopbackCapture::open(config, hooks)?))
        }
        LoopbackOpenMode::NativeOnly => {
            let mut native = NativeLoopbackCapture::try_open(config)?;
            native.set_hooks(hooks);
            Ok(AnyLoopback::Native(native))
        }
        LoopbackOpenMode::PreferNative => {
            if !NativeLoopbackCapture::is_available() {
                return Ok(AnyLoopback::Stub(StubLoopbackCapture::open(config, hooks)?));
            }
            // Single open when COM is linked — never probe then open again.
            match NativeLoopbackCapture::try_open(config.clone()) {
                Ok(mut native) => {
                    native.set_hooks(hooks);
                    Ok(AnyLoopback::Native(native))
                }
                Err(LoopbackError::NativeUnavailable(_)) => {
                    Ok(AnyLoopback::Stub(StubLoopbackCapture::open(config, hooks)?))
                }
                Err(e) => {
                    let mut hooks = hooks;
                    hooks.on_event(super::hooks::LoopbackEvent::ClientOpenFailed {
                        message: e.to_string(),
                    });
                    Ok(AnyLoopback::Stub(StubLoopbackCapture::open(config, hooks)?))
                }
            }
        }
    }
}

fn validate_config(config: &LoopbackConfig) -> Result<(), LoopbackError> {
    if config.sample_rate == 0 {
        return Err(LoopbackError::InvalidConfig("sample_rate == 0"));
    }
    if config.channels == 0 {
        return Err(LoopbackError::InvalidConfig("channels == 0"));
    }
    if config.packet_ms == 0 {
        return Err(LoopbackError::InvalidConfig("packet_ms == 0"));
    }
    let frames = config.sample_rate.saturating_mul(config.packet_ms) / 1000;
    if frames == 0 {
        return Err(LoopbackError::InvalidConfig(
            "packet too short for sample_rate",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasapi::hooks::{DeviceChangeReason, LoopbackEvent, RecordingHooks};
    use remotelink_media::source::AudioSource;

    #[test]
    fn stub_only_open_yields_pcm_packets() {
        let src = open_loopback(LoopbackConfig::synthetic()).unwrap();
        assert_eq!(src.backend_name(), "stub");
        let mut src = src;
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.frame_count(), 480); // 10 ms mono
    }

    #[test]
    fn native_only_open_or_client_error() {
        let cfg = LoopbackConfig {
            open_mode: LoopbackOpenMode::NativeOnly,
            ..LoopbackConfig::default()
        };
        match open_loopback(cfg) {
            Ok(src) => {
                assert_eq!(src.backend_name(), "wasapi");
            }
            Err(LoopbackError::ClientOpenFailed(_)) => {}
            Err(LoopbackError::NativeUnavailable(_)) if !cfg!(windows) => {}
            Err(e) => panic!("unexpected native-only error: {e}"),
        }
    }

    #[test]
    fn prefer_native_opens_wasapi_or_stub() {
        let cfg = LoopbackConfig {
            open_mode: LoopbackOpenMode::PreferNative,
            channels: 1,
            ..LoopbackConfig::default()
        };
        let src = open_loopback(cfg).unwrap();
        // On Windows with a render device → wasapi; headless / non-Windows → stub.
        assert!(
            src.backend_name() == "wasapi" || src.backend_name() == "stub",
            "backend={}",
            src.backend_name()
        );
    }

    #[test]
    fn device_change_hook_fires() {
        let hooks = RecordingHooks::new();
        let shared = hooks.shared_sink();
        let mut src = open_loopback_with_hooks(
            LoopbackConfig::synthetic(),
            Box::new(super::super::hooks::SharedHooks::new(shared)),
        )
        .unwrap();
        src.inject_device_change(
            DeviceChangeReason::DefaultDeviceChanged,
            Duration::from_millis(100),
        )
        .unwrap();
        let events = hooks.events();
        assert!(events.iter().any(|e| matches!(
            e,
            LoopbackEvent::DeviceChanged {
                reason: DeviceChangeReason::DefaultDeviceChanged
            }
        )));
    }
}
