//! Native WASAPI loopback skeleton (shared mode + LOOPBACK flag).
//!
//! # COM sequence (production, not yet linked)
//!
//! ```text
//! CoInitializeEx(COINIT_MULTITHREADED)
//! CoCreateInstance(MMDeviceEnumerator)
//! enumerator.GetDefaultAudioEndpoint(eRender, eConsole) -> IMMDevice
//! device.Activate(IAudioClient)
//! client.GetMixFormat() -> WAVEFORMATEX*
//! client.Initialize(
//!     AUDCLNT_SHAREMODE_SHARED,
//!     AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
//!     hnsBufferDuration,
//!     0,
//!     mixFormat,
//!     NULL,
//! )
//! client.GetService(IAudioCaptureClient)
//! client.GetService(IAudioClock)          // map positions → host_mono
//! client.Start()
//! loop: capture.GetBuffer / ReleaseBuffer → packetize 10 ms PCM s16
//! ```
//!
//! Device graph: register `IMMNotificationClient` on the enumerator; on default
//! device change stop/reopen and surface [`super::hooks::LoopbackEvent::DeviceChanged`].
//!
//! # This build
//!
//! COM / `windows` crate bindings are intentionally **not** pulled in yet
//! (MinGW CI + heavy COM surface). [`NativeLoopbackCapture::try_open`] returns
//! [`super::capture::LoopbackError::NativeUnavailable`]. Prefer
//! [`super::capture::open_loopback`] which falls back to the stub.
//!
//! Device-change on this skeleton returns [`LoopbackError::ReopenRequired`] so
//! the agent re-calls [`super::open_loopback`] rather than silently EOS-ing.

use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};

use super::capture::{LoopbackConfig, LoopbackError, LoopbackSource};
use super::hooks::{DeviceChangeReason, LoopbackEvent, LoopbackHooks};

/// Documented constant: WASAPI shared-mode loopback stream flag.
///
/// Value from `audioclient.h`: `AUDCLNT_STREAMFLAGS_LOOPBACK = 0x00020000`.
pub const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;

/// Shared-mode share mode enum discriminant (`AUDCLNT_SHAREMODE_SHARED = 0`).
pub const AUDCLNT_SHAREMODE_SHARED: u32 = 0;

/// Placeholder native capture handle.
///
/// Once COM is wired, this holds `IAudioClient` / `IAudioCaptureClient` and
/// implements pull-based 10 ms PCM delivery with `IAudioClock` PTS mapping.
pub struct NativeLoopbackCapture {
    sample_rate: u32,
    channels: u16,
    running: bool,
    hooks: Box<dyn LoopbackHooks>,
    /// Host-mono anchor for the current client start (PTS origin).
    start_pts: Duration,
    /// Reserved for future buffer/clock state.
    _packet_ms: u32,
}

impl std::fmt::Debug for NativeLoopbackCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLoopbackCapture")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.running)
            .field("start_pts", &self.start_pts)
            .finish_non_exhaustive()
    }
}

impl NativeLoopbackCapture {
    /// Whether the COM loopback path is linked in this build.
    ///
    /// Used by [`super::open_loopback_with_hooks`] for a single-open PreferNative
    /// path (no probe + second Initialize).
    pub fn is_available() -> bool {
        // Flip to true when COM IAudioClient open is implemented.
        false
    }

    /// Attempt to open the default render endpoint in shared loopback mode.
    ///
    /// Does **not** take hooks — caller installs them via [`Self::set_hooks`]
    /// after a successful open (single-open PreferNative policy).
    ///
    /// Currently always returns [`LoopbackError::NativeUnavailable`].
    pub fn try_open(config: LoopbackConfig) -> Result<Self, LoopbackError> {
        let _ = config;
        let _flags = AUDCLNT_STREAMFLAGS_LOOPBACK;
        let _mode = AUDCLNT_SHAREMODE_SHARED;
        Err(LoopbackError::NativeUnavailable(
            "COM IAudioClient loopback not linked in this build; use StubOnly / PreferNative",
        ))
    }

    /// Install event hooks after a successful open.
    pub fn set_hooks(&mut self, hooks: Box<dyn LoopbackHooks>) {
        self.hooks = hooks;
    }

    /// Construct a native handle for tests that need the type without COM.
    ///
    /// The handle is created in the **stopped** state and never yields frames
    /// until a real open path exists.
    pub fn skeleton_for_tests(config: LoopbackConfig, hooks: Box<dyn LoopbackHooks>) -> Self {
        Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            running: false,
            hooks,
            start_pts: Duration::from_millis(config.start_pts_ms),
            _packet_ms: config.packet_ms,
        }
    }
}

impl AudioSource for NativeLoopbackCapture {
    type Error = LoopbackError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        // Production: IAudioCaptureClient::GetBuffer → s16 packet + energy check.
        Err(LoopbackError::Capture(
            "native WASAPI capture pump not implemented".into(),
        ))
    }
}

impl LoopbackSource for NativeLoopbackCapture {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn backend_name(&self) -> &'static str {
        "wasapi"
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn inject_device_change(
        &mut self,
        reason: DeviceChangeReason,
        _new_start_pts: Duration,
    ) -> Result<(), LoopbackError> {
        self.hooks.on_event(LoopbackEvent::DeviceChanged { reason });
        // Production must Stop → release → reopen default endpoint → Start.
        // Skeleton cannot reopen: stop the pump and tell the agent to re-open.
        self.running = false;
        Err(LoopbackError::ReopenRequired)
    }

    fn inject_silence_packets(&mut self, _count: u32) {
        // No-op until pump exists; exclusive-mode detection is stub-tested.
    }
}

/// Map a WASAPI device position (from `IAudioClock::GetPosition`) to host mono.
///
/// `position_samples / frequency` gives seconds since client start; add the
/// host-mono anchor recorded at `IAudioClient::Start`.
pub fn position_to_host_mono(
    position_samples: u64,
    clock_frequency_hz: u64,
    client_start_host_mono: Duration,
) -> Duration {
    if clock_frequency_hz == 0 {
        return client_start_host_mono;
    }
    let nanos = position_samples.saturating_mul(1_000_000_000) / clock_frequency_hz;
    client_start_host_mono + Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasapi::hooks::NullHooks;
    use crate::wasapi::LoopbackConfig;

    #[test]
    fn loopback_flag_matches_sdk() {
        assert_eq!(AUDCLNT_STREAMFLAGS_LOOPBACK, 0x0002_0000);
        assert_eq!(AUDCLNT_SHAREMODE_SHARED, 0);
    }

    #[test]
    fn try_open_is_unavailable() {
        let err = NativeLoopbackCapture::try_open(LoopbackConfig::default()).unwrap_err();
        assert!(matches!(err, LoopbackError::NativeUnavailable(_)));
        assert!(!NativeLoopbackCapture::is_available());
    }

    #[test]
    fn position_mapping_10ms_at_48k() {
        let t0 = Duration::from_millis(100);
        // 480 samples @ 48 kHz = 10 ms
        let pts = position_to_host_mono(480, 48_000, t0);
        assert_eq!(pts, Duration::from_millis(110));
    }

    #[test]
    fn skeleton_next_frame_none_when_stopped() {
        let mut n = NativeLoopbackCapture::skeleton_for_tests(
            LoopbackConfig::default(),
            Box::new(NullHooks),
        );
        assert!(n.next_frame().unwrap().is_none());
    }

    #[test]
    fn device_change_requests_reopen() {
        let mut n = NativeLoopbackCapture::skeleton_for_tests(
            LoopbackConfig::default(),
            Box::new(NullHooks),
        );
        n.running = true;
        let err = n
            .inject_device_change(
                DeviceChangeReason::DefaultDeviceChanged,
                Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(err, LoopbackError::ReopenRequired));
        assert!(!n.is_running());
    }
}
