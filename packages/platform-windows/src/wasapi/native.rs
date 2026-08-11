//! Native WASAPI shared-mode loopback capture.
//!
//! # COM sequence
//!
//! ```text
//! CoInitializeEx(COINIT_MULTITHREADED)
//! CoCreateInstance(MMDeviceEnumerator)
//! enumerator.GetDefaultAudioEndpoint(eRender, eConsole) -> IMMDevice
//! device.Activate(IAudioClient)
//! client.GetMixFormat() -> WAVEFORMATEX*
//! client.Initialize(SHARED, LOOPBACK | AUTOCONVERTPCM, …)
//! client.GetService(IAudioCaptureClient)
//! client.Start()
//! loop: GetBuffer / ReleaseBuffer → packetize 10 ms PCM s16
//! ```
//!
//! Exclusive-mode games that starve loopback are detected via near-zero PCM
//! energy (same helper as the stub). Device-change still returns
//! [`LoopbackError::ReopenRequired`] so the agent re-opens.
//!
//! # Non-Windows
//!
//! [`try_open`] returns [`LoopbackError::NativeUnavailable`].

use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};

use super::capture::{LoopbackConfig, LoopbackError, LoopbackSource};
use super::energy::pcm_is_near_silence_default;
use super::hooks::{
    DeviceChangeReason, ExclusiveModeWarning, LoopbackEvent, LoopbackHooks, NullHooks,
};

/// Documented constant: WASAPI shared-mode loopback stream flag.
///
/// Value from `audioclient.h`: `AUDCLNT_STREAMFLAGS_LOOPBACK = 0x00020000`.
pub const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;

/// Shared-mode share mode enum discriminant (`AUDCLNT_SHAREMODE_SHARED = 0`).
pub const AUDCLNT_SHAREMODE_SHARED: u32 = 0;

/// Auto-convert mix format toward PCM (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`).
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;

/// Native WASAPI loopback handle (Windows COM; stub-unavailable elsewhere).
pub struct NativeLoopbackCapture {
    sample_rate: u32,
    channels: u16,
    packet_ms: u32,
    packet_frames: u32,
    running: bool,
    hooks: Box<dyn LoopbackHooks>,
    /// Host-mono anchor for the current client start (PTS origin).
    start_pts: Duration,
    samples_emitted: u64,
    /// Interleaved s16 samples not yet packetized.
    pending: Vec<i16>,
    silent_streak: u32,
    exclusive_silence_packets: u32,
    exclusive_warned: bool,
    #[cfg(windows)]
    com: Option<WindowsComState>,
}

#[cfg(windows)]
struct WindowsComState {
    // Keep client alive so capture remains valid.
    _client: windows::Win32::Media::Audio::IAudioClient,
    capture: windows::Win32::Media::Audio::IAudioCaptureClient,
    /// Mix format bits per sample (16 or 32).
    bits_per_sample: u16,
    /// True when mix format is IEEE float.
    is_float: bool,
    /// Frames currently held by GetBuffer awaiting ReleaseBuffer.
    outstanding_frames: u32,
}

// SAFETY: WASAPI shared-mode client/capture are used only from the agent media
// pump thread (no concurrent COM calls). Marking Send lets SessionManager live
// across tokio::spawn / await boundaries; callers must not race next_frame.
#[cfg(windows)]
unsafe impl Send for WindowsComState {}

// SAFETY: same single-threaded pump contract as WindowsComState.
unsafe impl Send for NativeLoopbackCapture {}

impl std::fmt::Debug for NativeLoopbackCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLoopbackCapture")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.running)
            .field("start_pts", &self.start_pts)
            .field("pending_samples", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl NativeLoopbackCapture {
    /// Whether the COM loopback path is linked in this build.
    pub fn is_available() -> bool {
        cfg!(windows)
    }

    /// Attempt to open the default render endpoint in shared loopback mode.
    ///
    /// Does **not** take hooks — caller installs them via [`Self::set_hooks`]
    /// after a successful open (single-open PreferNative policy).
    pub fn try_open(config: LoopbackConfig) -> Result<Self, LoopbackError> {
        #[cfg(windows)]
        {
            windows_try_open(config)
        }
        #[cfg(not(windows))]
        {
            let _ = config;
            Err(LoopbackError::NativeUnavailable(
                "WASAPI loopback is only available on Windows",
            ))
        }
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
        let packet_frames = config.sample_rate.saturating_mul(config.packet_ms) / 1000;
        let exclusive_silence_packets = config
            .exclusive_silence_ms
            .div_ceil(u64::from(config.packet_ms.max(1)))
            .max(1) as u32;
        Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            packet_ms: config.packet_ms,
            packet_frames: packet_frames.max(1),
            running: false,
            hooks,
            start_pts: Duration::from_millis(config.start_pts_ms),
            samples_emitted: 0,
            pending: Vec::new(),
            silent_streak: 0,
            exclusive_silence_packets,
            exclusive_warned: false,
            #[cfg(windows)]
            com: None,
        }
    }

    fn pts_for_sample_index(&self, index: u64) -> Duration {
        let ns = index.saturating_mul(1_000_000_000) / u64::from(self.sample_rate.max(1));
        self.start_pts + Duration::from_nanos(ns)
    }

    fn note_energy(&mut self, silent: bool) {
        if silent {
            self.silent_streak = self.silent_streak.saturating_add(1);
            if !self.exclusive_warned && self.silent_streak >= self.exclusive_silence_packets {
                self.exclusive_warned = true;
                let sustained_ms = u64::from(self.silent_streak) * u64::from(self.packet_ms);
                self.hooks.on_event(LoopbackEvent::ExclusiveMode {
                    warning: ExclusiveModeWarning {
                        sustained_silence_ms: sustained_ms,
                        message: "loopback near-zero energy (possible exclusive-mode audio)".into(),
                    },
                });
            }
        } else {
            self.silent_streak = 0;
            self.exclusive_warned = false;
        }
    }

    fn take_packet(&mut self) -> Option<AudioFrame> {
        let need = self.packet_frames as usize * self.channels as usize;
        if self.pending.len() < need {
            return None;
        }
        let pcm: Vec<i16> = self.pending.drain(..need).collect();
        let pts = self.pts_for_sample_index(self.samples_emitted);
        self.samples_emitted = self
            .samples_emitted
            .saturating_add(u64::from(self.packet_frames));
        let silent = pcm_is_near_silence_default(&pcm);
        self.note_energy(silent);
        Some(AudioFrame::from_s16(
            pts,
            self.sample_rate,
            self.channels,
            pcm,
        ))
    }

    #[cfg(windows)]
    fn pull_from_device(&mut self) -> Result<(), LoopbackError> {
        let com = match self.com.as_mut() {
            Some(c) => c,
            None => return Ok(()),
        };
        // Release any previous buffer first.
        if com.outstanding_frames > 0 {
            unsafe {
                let _ = com.capture.ReleaseBuffer(com.outstanding_frames);
            }
            com.outstanding_frames = 0;
        }

        // Drain available packets (bounded iterations per pull).
        for _ in 0..32 {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            let hr = unsafe {
                com.capture.GetBuffer(
                    &mut data_ptr,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )
            };
            if hr.is_err() || num_frames == 0 || data_ptr.is_null() {
                break;
            }
            com.outstanding_frames = num_frames;
            let silent_flag = (flags
                & windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                != 0;

            let ch = self.channels as usize;
            let frames = num_frames as usize;
            if silent_flag {
                self.pending
                    .extend(std::iter::repeat_n(0i16, frames * ch));
            } else {
                append_pcm_s16(
                    &mut self.pending,
                    data_ptr,
                    frames,
                    ch,
                    com.bits_per_sample,
                    com.is_float,
                );
            }
            unsafe {
                let _ = com.capture.ReleaseBuffer(num_frames);
            }
            com.outstanding_frames = 0;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn append_pcm_s16(
    out: &mut Vec<i16>,
    data: *mut u8,
    frames: usize,
    channels: usize,
    bits: u16,
    is_float: bool,
) {
    let samples = frames.saturating_mul(channels);
    if is_float || bits == 32 {
        // IEEE float32 interleaved.
        let slice = unsafe { std::slice::from_raw_parts(data as *const f32, samples) };
        for &s in slice {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            out.push(v);
        }
    } else {
        // PCM s16 interleaved.
        let slice = unsafe { std::slice::from_raw_parts(data as *const i16, samples) };
        out.extend_from_slice(slice);
    }
}

#[cfg(windows)]
fn windows_try_open(config: LoopbackConfig) -> Result<NativeLoopbackCapture, LoopbackError> {
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // WAVE_FORMAT_IEEE_FLOAT / EXTENSIBLE (not always re-exported by windows crate).
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    // COM init is process-global; S_FALSE / RPC_E_CHANGED_MODE are acceptable.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| {
            LoopbackError::ClientOpenFailed(format!("MMDeviceEnumerator: {e}"))
        })?
    };

    let device = unsafe {
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| LoopbackError::ClientOpenFailed(format!("GetDefaultAudioEndpoint: {e}")))?
    };

    let client: IAudioClient = unsafe {
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| LoopbackError::ClientOpenFailed(format!("Activate IAudioClient: {e}")))?
    };

    let mix_fmt_ptr = unsafe {
        client
            .GetMixFormat()
            .map_err(|e| LoopbackError::ClientOpenFailed(format!("GetMixFormat: {e}")))?
    };
    if mix_fmt_ptr.is_null() {
        return Err(LoopbackError::ClientOpenFailed(
            "GetMixFormat returned null".into(),
        ));
    }

    // Copy fields we need before freeing.
    let (sample_rate, channels, bits, is_float) = unsafe {
        let f = &*mix_fmt_ptr;
        let is_float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT
            || (f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && f.wBitsPerSample == 32);
        (
            f.nSamplesPerSec,
            f.nChannels,
            f.wBitsPerSample,
            is_float,
        )
    };

    // Prefer device mix format with loopback + autoconvert when possible.
    // Buffer: 100 ms of hns (100-ns units).
    let buffer_hns: i64 = 1_000_000; // 100 ms
    let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;

    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            buffer_hns,
            0,
            mix_fmt_ptr,
            None,
        )
    };
    unsafe {
        CoTaskMemFree(Some(mix_fmt_ptr as *const _));
    }
    init.map_err(|e| LoopbackError::ClientOpenFailed(format!("IAudioClient::Initialize: {e}")))?;

    let capture: IAudioCaptureClient = unsafe {
        client
            .GetService::<IAudioCaptureClient>()
            .map_err(|e| LoopbackError::ClientOpenFailed(format!("GetService capture: {e}")))?
    };

    unsafe {
        client
            .Start()
            .map_err(|e| LoopbackError::ClientOpenFailed(format!("IAudioClient::Start: {e}")))?;
    }

    // Use device mix rate/channels; config packet_ms for packetization.
    let sample_rate = if sample_rate == 0 {
        config.sample_rate
    } else {
        sample_rate
    };
    let channels = if channels == 0 {
        config.channels
    } else {
        channels
    };
    let packet_frames = sample_rate.saturating_mul(config.packet_ms) / 1000;
    let exclusive_silence_packets = config
        .exclusive_silence_ms
        .div_ceil(u64::from(config.packet_ms.max(1)))
        .max(1) as u32;

    Ok(NativeLoopbackCapture {
        sample_rate,
        channels,
        packet_ms: config.packet_ms,
        packet_frames: packet_frames.max(1),
        running: true,
        hooks: Box::new(NullHooks),
        start_pts: Duration::from_millis(config.start_pts_ms),
        samples_emitted: 0,
        pending: Vec::with_capacity(packet_frames as usize * channels as usize * 4),
        silent_streak: 0,
        exclusive_silence_packets,
        exclusive_warned: false,
        com: Some(WindowsComState {
            _client: client,
            capture,
            bits_per_sample: if bits == 0 { 32 } else { bits },
            is_float,
            outstanding_frames: 0,
        }),
    })
}

#[cfg(windows)]
impl Drop for NativeLoopbackCapture {
    fn drop(&mut self) {
        self.running = false;
        if let Some(com) = self.com.as_mut() {
            if com.outstanding_frames > 0 {
                unsafe {
                    let _ = com.capture.ReleaseBuffer(com.outstanding_frames);
                }
                com.outstanding_frames = 0;
            }
            // IAudioClient::Stop via _client drop order: capture then client.
            // Explicit stop when possible.
            // _client is private; Stop on drop of COM objects is fine.
        }
        self.com = None;
    }
}

impl AudioSource for NativeLoopbackCapture {
    type Error = LoopbackError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        #[cfg(windows)]
        {
            self.pull_from_device()?;
            if let Some(frame) = self.take_packet() {
                return Ok(Some(frame));
            }
            // No full packet yet — brief spin of empty pulls is idle.
            return Ok(None);
        }
        #[cfg(not(windows))]
        {
            Ok(None)
        }
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
        #[cfg(windows)]
        if let Some(com) = self.com.as_mut() {
            if com.outstanding_frames > 0 {
                unsafe {
                    let _ = com.capture.ReleaseBuffer(com.outstanding_frames);
                }
                com.outstanding_frames = 0;
            }
        }
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
        self.stop();
        Err(LoopbackError::ReopenRequired)
    }

    fn inject_silence_packets(&mut self, count: u32) {
        // Inject digital silence into the pending queue for exclusive-mode tests.
        let need = count as usize * self.packet_frames as usize * self.channels as usize;
        self.pending.extend(std::iter::repeat_n(0i16, need));
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
    fn is_available_matches_platform() {
        assert_eq!(NativeLoopbackCapture::is_available(), cfg!(windows));
    }

    #[test]
    fn try_open_on_this_machine() {
        // Headless CI may lack a render endpoint → ClientOpenFailed is OK.
        // Success means COM path is live.
        match NativeLoopbackCapture::try_open(LoopbackConfig::default()) {
            Ok(mut n) => {
                assert_eq!(n.backend_name(), "wasapi");
                assert!(n.is_running());
                // May be None if no audio is playing yet (empty buffer).
                let _ = n.next_frame();
                n.stop();
            }
            Err(LoopbackError::ClientOpenFailed(_)) => {}
            Err(LoopbackError::NativeUnavailable(_)) if !cfg!(windows) => {}
            Err(e) => panic!("unexpected try_open error: {e}"),
        }
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
