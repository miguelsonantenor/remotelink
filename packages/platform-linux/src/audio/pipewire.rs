//! Native PipeWire / Pulse **monitor** skeleton (system audio, not mic).
//!
//! # Production sequence (not yet linked)
//!
//! ```text
//! # Preferred: PipeWire
//! pw_init / pw_context_connect
//! Find default sink monitor node (or config.sink_name.monitor)
//! pw_stream_new (SPA_AUDIO_format S16 / F32, 48 kHz, stereo)
//! On process: map buffer → packetize 10 ms PCM → AudioFrame
//!
//! # Fallback: PulseAudio simple / introspect
//! pa_context_get_server_info → default_sink_name
//! open "{sink}.monitor" with PA_STREAM_RECORD
//! read → S16LE packetize
//! ```
//!
//! Device-change: subscribe to sink default changes; agent re-opens and emits
//! `media_restart` (same contract as WASAPI loopback on Windows).
//!
//! # This build
//!
//! Native libraries are **not** linked. [`NativePipeWireMonitor::try_open`]
//! returns [`MonitorError::NativeUnavailable`] or [`MonitorError::Unsupported`].
//! Prefer [`super::open_monitor`] which falls back to the mock under
//! [`MonitorOpenMode::PreferNative`].

use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};

use super::monitor::{MonitorConfig, MonitorError, MonitorSource};

/// Placeholder native monitor capture handle.
#[derive(Debug)]
pub struct NativePipeWireMonitor {
    sample_rate: u32,
    channels: u16,
    running: bool,
    start_pts: Duration,
    packet_ms: u32,
}

impl NativePipeWireMonitor {
    /// Whether the PipeWire/Pulse monitor path is linked in this build.
    pub fn is_available() -> bool {
        // Flip to true when libpipewire / libpulse open is implemented.
        false
    }

    /// Attempt to open the default (or named) sink monitor stream.
    pub fn try_open(config: MonitorConfig) -> Result<Self, MonitorError> {
        let _ = config.sink_name;
        if !cfg!(target_os = "linux") {
            return Err(MonitorError::Unsupported);
        }
        let _ = config;
        Err(MonitorError::NativeUnavailable(
            "libpipewire / libpulse monitor not linked in this build; use StubOnly / PreferNative",
        ))
    }

    /// Construct a stopped skeleton for type-level tests (no frames).
    pub fn skeleton_for_tests(config: MonitorConfig) -> Self {
        Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            running: false,
            start_pts: Duration::from_millis(config.start_pts_ms),
            packet_ms: config.packet_ms,
        }
    }

    /// Packetization interval (ms).
    pub fn packet_ms(&self) -> u32 {
        self.packet_ms
    }
}

impl AudioSource for NativePipeWireMonitor {
    type Error = MonitorError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        Err(MonitorError::Capture(
            "pipewire/pulse monitor stream not linked".into(),
        ))
    }
}

impl MonitorSource for NativePipeWireMonitor {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn backend_name(&self) -> &'static str {
        "pipewire-monitor"
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn start_pts(&self) -> Duration {
        self.start_pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_open_fails_structured() {
        let err = NativePipeWireMonitor::try_open(MonitorConfig::default()).unwrap_err();
        #[cfg(target_os = "linux")]
        {
            assert!(matches!(err, MonitorError::NativeUnavailable(_)));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(matches!(err, MonitorError::Unsupported));
        }
    }

    #[test]
    fn not_available_until_linked() {
        assert!(!NativePipeWireMonitor::is_available());
    }

    #[test]
    fn skeleton_yields_none_when_stopped() {
        let mut m = NativePipeWireMonitor::skeleton_for_tests(MonitorConfig::synthetic());
        assert_eq!(m.backend_name(), "pipewire-monitor");
        assert!(!m.is_running());
        assert!(m.next_frame().unwrap().is_none());
    }
}
