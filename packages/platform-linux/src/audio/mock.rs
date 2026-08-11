//! Software audio-monitor mock for CI and units (no PipeWire / Pulse).

use std::f64::consts::TAU;
use std::time::Duration;

use remotelink_media::source::{AudioFrame, AudioSource};

use super::monitor::{MonitorConfig, MonitorError, MonitorSource};

/// Synthetic monitor that implements the same pull API as native PipeWire/Pulse.
///
/// Generates a low-amplitude tone by default so energy-based detectors and Opus
/// paths have non-silent PCM without a real sink.
pub struct MockMonitorSource {
    sample_rate: u32,
    channels: u16,
    packet_ms: u32,
    packet_frames: u32,
    sample_index: u64,
    start_pts: Duration,
    running: bool,
    frequency_hz: f64,
    amplitude: f64,
}

impl std::fmt::Debug for MockMonitorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockMonitorSource")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.running)
            .field("start_pts", &self.start_pts)
            .finish_non_exhaustive()
    }
}

impl MockMonitorSource {
    /// Open a mock monitor with the given config.
    pub fn open(config: MonitorConfig) -> Result<Self, MonitorError> {
        let packet_frames = config.sample_rate * config.packet_ms / 1000;
        if packet_frames == 0 {
            return Err(MonitorError::InvalidConfig("packet_frames == 0"));
        }
        Ok(Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            packet_ms: config.packet_ms,
            packet_frames,
            sample_index: 0,
            start_pts: Duration::from_millis(config.start_pts_ms),
            running: true,
            frequency_hz: 440.0,
            amplitude: 0.15,
        })
    }

    /// Packet duration in milliseconds.
    pub fn packet_ms(&self) -> u32 {
        self.packet_ms
    }

    fn pts_for_sample_index(&self, index: u64) -> Duration {
        let ns = index.saturating_mul(1_000_000_000) / u64::from(self.sample_rate);
        self.start_pts + Duration::from_nanos(ns)
    }

    fn render_packet(&mut self) -> AudioFrame {
        let n = self.packet_frames as usize;
        let ch = self.channels as usize;
        let mut pcm = Vec::with_capacity(n * ch);
        let pts = self.pts_for_sample_index(self.sample_index);
        for i in 0..n {
            let t = (self.sample_index + i as u64) as f64 / f64::from(self.sample_rate);
            let s = (self.amplitude * (TAU * self.frequency_hz * t).sin() * i16::MAX as f64) as i16;
            for _ in 0..ch {
                pcm.push(s);
            }
        }
        self.sample_index += u64::from(self.packet_frames);
        AudioFrame::from_s16(pts, self.sample_rate, self.channels, pcm)
    }
}

impl AudioSource for MockMonitorSource {
    type Error = MonitorError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if !self.running {
            return Ok(None);
        }
        Ok(Some(self.render_packet()))
    }
}

impl MonitorSource for MockMonitorSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn backend_name(&self) -> &'static str {
        "mock"
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
    use remotelink_media::source::AudioSource;

    #[test]
    fn tone_packets_are_10ms() {
        let mut src = MockMonitorSource::open(MonitorConfig::synthetic()).unwrap();
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.duration(), Duration::from_millis(10));
        assert!(f.pcm_s16.iter().any(|&s| s != 0));
        assert_eq!(f.pts_host_mono, Duration::ZERO);
        let f2 = src.next_frame().unwrap().unwrap();
        assert_eq!(f2.pts_host_mono, Duration::from_millis(10));
    }

    #[test]
    fn stop_ends_stream() {
        let mut src = MockMonitorSource::open(MonitorConfig::synthetic()).unwrap();
        src.stop();
        assert!(!src.is_running());
        assert!(src.next_frame().unwrap().is_none());
    }

    #[test]
    fn custom_start_pts() {
        let cfg = MonitorConfig {
            start_pts_ms: 250,
            ..MonitorConfig::synthetic()
        };
        let mut src = MockMonitorSource::open(cfg).unwrap();
        assert_eq!(src.start_pts(), Duration::from_millis(250));
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.pts_host_mono, Duration::from_millis(250));
    }
}
