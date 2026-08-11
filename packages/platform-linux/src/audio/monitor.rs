//! Audio monitor capture configuration and open helpers.
//!
//! Linux system audio uses a **monitor** of the default (or selected) sink —
//! equivalent in role to WASAPI loopback on Windows. This is not a microphone
//! capture path.

use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};
use thiserror::Error;

use super::mock::MockMonitorSource;
use super::pipewire::NativePipeWireMonitor;

/// RemoteLink default capture sample rate (Hz).
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// Default channel count (stereo monitor; mono optional).
pub const DEFAULT_CHANNELS: u16 = 2;
/// Opus packet duration used by the host path (ms).
pub const DEFAULT_PACKET_MS: u32 = 10;

/// How [`open_monitor`] should select a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorOpenMode {
    /// Prefer native PipeWire/Pulse monitor; fall back to mock if unavailable.
    #[default]
    PreferNative,
    /// Always use the CI/synthetic mock (no PipeWire).
    StubOnly,
    /// Require native monitor (error if unavailable).
    NativeOnly,
}

/// Configuration for opening a system-audio monitor source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    /// Sample rate in Hz (default 48_000).
    pub sample_rate: u32,
    /// Channel count (1 or 2).
    pub channels: u16,
    /// Packetization interval in milliseconds (typically 10).
    pub packet_ms: u32,
    /// Backend selection policy.
    pub open_mode: MonitorOpenMode,
    /// Host-mono PTS origin for the first sample (media epoch `t0` candidate).
    pub start_pts_ms: u64,
    /// Optional sink name / node target (None = default sink monitor).
    pub sink_name: Option<String>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            packet_ms: DEFAULT_PACKET_MS,
            open_mode: MonitorOpenMode::PreferNative,
            start_pts_ms: 0,
            sink_name: None,
        }
    }
}

impl MonitorConfig {
    /// CI / unit-test defaults: stub backend, mono A440-friendly layout.
    pub fn synthetic() -> Self {
        Self {
            open_mode: MonitorOpenMode::StubOnly,
            channels: 1,
            ..Self::default()
        }
    }
}

/// Errors from opening or reading monitor capture.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MonitorError {
    /// Invalid configuration values.
    #[error("invalid monitor config: {0}")]
    InvalidConfig(&'static str),
    /// Native PipeWire/Pulse path is not linked / not available on this build.
    #[error("native pipewire/pulse monitor unavailable: {0}")]
    NativeUnavailable(&'static str),
    /// Opening the monitor stream failed (missing sink, permission, …).
    #[error("monitor open failed: {0}")]
    ClientOpenFailed(String),
    /// Capture read failed after open.
    #[error("monitor capture failed: {0}")]
    Capture(String),
    /// Monitor path is unsupported on this platform/build.
    #[error("audio monitor unsupported on this platform/build")]
    Unsupported,
}

/// Type-erased monitor source implementing [`AudioSource`].
///
/// Both mock and (future) native backends expose the same pull API so the
/// session agent can feed PCM into Opus encoding.
pub trait MonitorSource: AudioSource<Error = MonitorError> {
    /// Configured sample rate.
    fn sample_rate(&self) -> u32;
    /// Configured channel count.
    fn channels(&self) -> u16;
    /// Backend name for logs (`"mock"` / `"pipewire-monitor"`).
    fn backend_name(&self) -> &'static str;
    /// Stop capture; further `next_frame` returns `Ok(None)`.
    fn stop(&mut self);
    /// Whether capture is still running.
    fn is_running(&self) -> bool;
    /// Host-mono PTS origin of the current capture timeline.
    fn start_pts(&self) -> Duration;
}

/// Concrete backend used by the agent media plane.
pub enum AnyMonitor {
    /// Software mock (always available).
    Mock(MockMonitorSource),
    /// Native PipeWire/Pulse skeleton (open may fail until linked).
    Native(NativePipeWireMonitor),
}

impl std::fmt::Debug for AnyMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mock(_) => f.write_str("AnyMonitor::Mock(..)"),
            Self::Native(_) => f.write_str("AnyMonitor::Native(..)"),
        }
    }
}

impl AudioSource for AnyMonitor {
    type Error = MonitorError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        match self {
            Self::Mock(s) => s.next_frame(),
            Self::Native(n) => n.next_frame(),
        }
    }
}

impl MonitorSource for AnyMonitor {
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Mock(s) => s.sample_rate(),
            Self::Native(n) => n.sample_rate(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            Self::Mock(s) => s.channels(),
            Self::Native(n) => n.channels(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            Self::Mock(s) => s.backend_name(),
            Self::Native(n) => n.backend_name(),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Mock(s) => s.stop(),
            Self::Native(n) => n.stop(),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            Self::Mock(s) => s.is_running(),
            Self::Native(n) => n.is_running(),
        }
    }

    fn start_pts(&self) -> Duration {
        match self {
            Self::Mock(s) => s.start_pts(),
            Self::Native(n) => n.start_pts(),
        }
    }
}

/// Open a system-audio monitor with default (null) hooks / naming.
pub fn open_monitor(config: MonitorConfig) -> Result<AnyMonitor, MonitorError> {
    open_monitor_with_name(config)
}

/// Open a system-audio monitor (sink name taken from `config.sink_name`).
pub fn open_monitor_with_name(config: MonitorConfig) -> Result<AnyMonitor, MonitorError> {
    validate_config(&config)?;
    match config.open_mode {
        MonitorOpenMode::StubOnly => {
            let m = MockMonitorSource::open(config)?;
            Ok(AnyMonitor::Mock(m))
        }
        MonitorOpenMode::NativeOnly => {
            let n = NativePipeWireMonitor::try_open(config)?;
            Ok(AnyMonitor::Native(n))
        }
        MonitorOpenMode::PreferNative => {
            if NativePipeWireMonitor::is_available() {
                match NativePipeWireMonitor::try_open(config.clone()) {
                    Ok(n) => Ok(AnyMonitor::Native(n)),
                    Err(_) => {
                        let m = MockMonitorSource::open(config)?;
                        Ok(AnyMonitor::Mock(m))
                    }
                }
            } else {
                let m = MockMonitorSource::open(config)?;
                Ok(AnyMonitor::Mock(m))
            }
        }
    }
}

fn validate_config(config: &MonitorConfig) -> Result<(), MonitorError> {
    if config.sample_rate == 0 {
        return Err(MonitorError::InvalidConfig("sample_rate == 0"));
    }
    if config.channels == 0 {
        return Err(MonitorError::InvalidConfig("channels == 0"));
    }
    if config.packet_ms == 0 {
        return Err(MonitorError::InvalidConfig("packet_ms == 0"));
    }
    let packet_frames = config.sample_rate.saturating_mul(config.packet_ms) / 1000;
    if packet_frames == 0 {
        return Err(MonitorError::InvalidConfig("packet_frames == 0"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_only_opens_mock() {
        let m = open_monitor(MonitorConfig::synthetic()).unwrap();
        assert_eq!(m.backend_name(), "mock");
        assert!(m.is_running());
        assert_eq!(m.sample_rate(), DEFAULT_SAMPLE_RATE);
        assert_eq!(m.channels(), 1);
    }

    #[test]
    fn prefer_native_falls_back_to_mock() {
        let m = open_monitor(MonitorConfig::default()).unwrap();
        assert_eq!(m.backend_name(), "mock");
    }

    #[test]
    fn native_only_errors_until_linked() {
        let cfg = MonitorConfig {
            open_mode: MonitorOpenMode::NativeOnly,
            ..MonitorConfig::default()
        };
        let err = open_monitor(cfg).unwrap_err();
        assert!(matches!(
            err,
            MonitorError::NativeUnavailable(_) | MonitorError::Unsupported
        ));
    }

    #[test]
    fn invalid_config_rejected() {
        let cfg = MonitorConfig {
            sample_rate: 0,
            open_mode: MonitorOpenMode::StubOnly,
            ..MonitorConfig::default()
        };
        assert!(matches!(
            open_monitor(cfg),
            Err(MonitorError::InvalidConfig(_))
        ));
    }
}
