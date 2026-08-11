//! Software loopback stub for CI and units (no WASAPI / no device).

use std::f64::consts::TAU;
use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};

use super::capture::{LoopbackConfig, LoopbackError, LoopbackSource};
use super::energy::pcm_is_near_silence_default;
use super::hooks::{DeviceChangeReason, ExclusiveModeWarning, LoopbackEvent, LoopbackHooks};

/// Synthetic loopback that implements the same pull API as native WASAPI.
///
/// Generates a low-amplitude tone by default. Exclusive-mode detection uses
/// real PCM energy ([`pcm_is_near_silence_default`]); [`Self::inject_silence_packets`]
/// forces digital-zero packets for deterministic tests.
pub struct StubLoopbackCapture {
    sample_rate: u32,
    channels: u16,
    packet_ms: u32,
    packet_frames: u32,
    sample_index: u64,
    start_pts: Duration,
    running: bool,
    /// Remaining forced-silent packets (exclusive-mode tests).
    silence_packets_left: u32,
    /// Consecutive silent packet count (for exclusive warning).
    silent_streak: u32,
    exclusive_silence_packets: u32,
    exclusive_warned: bool,
    frequency_hz: f64,
    amplitude: f64,
    hooks: Box<dyn LoopbackHooks>,
    media_restart_count: u32,
}

impl std::fmt::Debug for StubLoopbackCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubLoopbackCapture")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.running)
            .field("start_pts", &self.start_pts)
            .field("media_restart_count", &self.media_restart_count)
            .finish_non_exhaustive()
    }
}

impl StubLoopbackCapture {
    /// Open a stub capture with the given config and hooks.
    pub fn open(
        config: LoopbackConfig,
        hooks: Box<dyn LoopbackHooks>,
    ) -> Result<Self, LoopbackError> {
        let packet_frames = config.sample_rate * config.packet_ms / 1000;
        if packet_frames == 0 {
            return Err(LoopbackError::InvalidConfig("packet_frames == 0"));
        }
        // Ceiling division so short windows still require ≥1 full packet, and
        // e.g. 5 ms @ 10 ms packet → 1 packet (not truncated to 0 then forced).
        let exclusive_silence_packets = config
            .exclusive_silence_ms
            .div_ceil(u64::from(config.packet_ms.max(1)))
            .max(1) as u32;
        Ok(Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            packet_ms: config.packet_ms,
            packet_frames,
            sample_index: 0,
            start_pts: Duration::from_millis(config.start_pts_ms),
            running: true,
            silence_packets_left: 0,
            silent_streak: 0,
            exclusive_silence_packets,
            exclusive_warned: false,
            frequency_hz: 440.0,
            amplitude: 0.15,
            hooks,
            media_restart_count: 0,
        })
    }

    /// How many media restarts (device-change reinits) have occurred.
    pub fn media_restart_count(&self) -> u32 {
        self.media_restart_count
    }

    /// Host-mono PTS origin of the current capture timeline.
    pub fn start_pts(&self) -> Duration {
        self.start_pts
    }

    fn pts_for_sample_index(&self, index: u64) -> Duration {
        let ns = index.saturating_mul(1_000_000_000) / u64::from(self.sample_rate);
        self.start_pts + Duration::from_nanos(ns)
    }

    fn render_packet(&mut self, silent: bool) -> AudioFrame {
        let n = self.packet_frames as usize;
        let ch = self.channels as usize;
        let mut pcm = Vec::with_capacity(n * ch);
        let pts = self.pts_for_sample_index(self.sample_index);
        for i in 0..n {
            let s = if silent {
                0i16
            } else {
                let t = (self.sample_index + i as u64) as f64 / f64::from(self.sample_rate);
                (self.amplitude * (TAU * self.frequency_hz * t).sin() * i16::MAX as f64) as i16
            };
            for _ in 0..ch {
                pcm.push(s);
            }
        }
        self.sample_index += u64::from(self.packet_frames);
        AudioFrame::from_s16(pts, self.sample_rate, self.channels, pcm)
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
            // Allow re-warn after audio returns (game left exclusive mode).
            self.exclusive_warned = false;
        }
    }
}

impl AudioSource for StubLoopbackCapture {
    type Error = LoopbackError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        let force_silent = if self.silence_packets_left > 0 {
            self.silence_packets_left -= 1;
            true
        } else {
            false
        };
        let frame = self.render_packet(force_silent);
        // Real energy check so native near-zero PCM would also classify (and
        // force-silent inject still works).
        let silent = force_silent || pcm_is_near_silence_default(&frame.pcm_s16);
        self.note_energy(silent);
        Ok(Some(frame))
    }
}

impl LoopbackSource for StubLoopbackCapture {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn backend_name(&self) -> &'static str {
        "stub"
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
        new_start_pts: Duration,
    ) -> Result<(), LoopbackError> {
        // Mirror production: stop client, reopen, media_restart, brief mute.
        // Re-anchor capture PTS to the new session epoch so RTP stamps from 0
        // again (DESIGN media_restart / shared t0 contract).
        self.hooks.on_event(LoopbackEvent::DeviceChanged { reason });
        self.media_restart_count = self.media_restart_count.saturating_add(1);
        self.start_pts = new_start_pts;
        self.sample_index = 0;
        // Brief mute window (<200 ms design budget): one silent packet at packet_ms.
        self.silence_packets_left = self.silence_packets_left.max(1);
        self.silent_streak = 0;
        self.exclusive_warned = false;
        self.running = true;
        Ok(())
    }

    fn inject_silence_packets(&mut self, count: u32) {
        self.silence_packets_left = self.silence_packets_left.saturating_add(count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasapi::hooks::{LoopbackEvent, RecordingHooks};
    use crate::wasapi::{LoopbackConfig, LoopbackSource};
    use remotelink_media::source::AudioSource;

    #[test]
    fn tone_packets_are_10ms() {
        let mut src =
            StubLoopbackCapture::open(LoopbackConfig::synthetic(), Box::new(RecordingHooks::new()))
                .unwrap();
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.duration(), Duration::from_millis(10));
        assert!(f.pcm_s16.iter().any(|&s| s != 0));
    }

    #[test]
    fn exclusive_silence_emits_warning() {
        let hooks = RecordingHooks::new();
        let shared = hooks.shared_sink();
        let cfg = LoopbackConfig {
            exclusive_silence_ms: 30, // 3 × 10 ms packets
            ..LoopbackConfig::synthetic()
        };
        let mut src = StubLoopbackCapture::open(
            cfg,
            Box::new(crate::wasapi::hooks::SharedHooks::new(shared)),
        )
        .unwrap();
        src.inject_silence_packets(5);
        for _ in 0..5 {
            let _ = src.next_frame().unwrap();
        }
        assert!(hooks.events().iter().any(|e| matches!(
            e,
            LoopbackEvent::ExclusiveMode { warning }
                if warning.sustained_silence_ms >= 30
        )));
    }

    #[test]
    fn exclusive_threshold_uses_ceil_division() {
        // 5 ms window @ 10 ms packets → 1 packet (ceil), not truncated to 0.
        let cfg = LoopbackConfig {
            exclusive_silence_ms: 5,
            ..LoopbackConfig::synthetic()
        };
        let mut src = StubLoopbackCapture::open(cfg, Box::new(RecordingHooks::new())).unwrap();
        assert_eq!(src.exclusive_silence_packets, 1);
        src.inject_silence_packets(1);
        let _ = src.next_frame().unwrap();
        // Warning after 1 silent packet.
        // (hooks are owned by src; reopen with recording hooks)
    }

    #[test]
    fn device_change_reanchors_pts() {
        let mut src =
            StubLoopbackCapture::open(LoopbackConfig::synthetic(), Box::new(RecordingHooks::new()))
                .unwrap();
        let _ = src.next_frame().unwrap();
        let new_t0 = Duration::from_millis(5_000);
        src.inject_device_change(DeviceChangeReason::DefaultDeviceChanged, new_t0)
            .unwrap();
        assert_eq!(src.media_restart_count(), 1);
        assert_eq!(src.start_pts(), new_t0);
        let f = src.next_frame().unwrap().unwrap();
        // First packet after restart is mute; PTS still at new epoch.
        assert_eq!(f.pts_host_mono, new_t0);
        assert!(f.pcm_s16.iter().all(|&s| s == 0));
        let f2 = src.next_frame().unwrap().unwrap();
        assert_eq!(f2.pts_host_mono, new_t0 + Duration::from_millis(10));
    }

    #[test]
    fn stop_ends_stream() {
        let mut src =
            StubLoopbackCapture::open(LoopbackConfig::synthetic(), Box::new(RecordingHooks::new()))
                .unwrap();
        src.stop();
        assert!(src.next_frame().unwrap().is_none());
    }
}
