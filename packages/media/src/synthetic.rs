//! Synthetic video color-bars and audio tone generators for tests / CI.
//!
//! Both implement the capture traits and stamp frames with advancing
//! host-monotonic PTS from a controllable clock base.

use std::f64::consts::TAU;
use std::time::Duration;

use crate::source::{AudioFrame, AudioSource, PixelFormat, VideoFrame, VideoSource};

/// Error from synthetic sources (currently infallible operations only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticError;

impl std::fmt::Display for SyntheticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "synthetic source error")
    }
}

impl std::error::Error for SyntheticError {}

/// Vertical SMPTE-like color bars (RGB24) advancing at a fixed frame interval.
#[derive(Debug, Clone)]
pub struct SyntheticVideoBars {
    width: u32,
    height: u32,
    frame_interval: Duration,
    /// PTS of the next frame to emit.
    next_pts: Duration,
    frames_emitted: u64,
    /// Optional cap; `None` = infinite.
    max_frames: Option<u64>,
}

impl SyntheticVideoBars {
    /// Create a bars generator.
    ///
    /// - `width` / `height`: frame size (must be > 0)
    /// - `fps`: frames per second (must be > 0)
    /// - `start_pts`: host-mono PTS of the first frame
    pub fn new(width: u32, height: u32, fps: u32, start_pts: Duration) -> Self {
        assert!(width > 0 && height > 0 && fps > 0);
        Self {
            width,
            height,
            frame_interval: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            next_pts: start_pts,
            frames_emitted: 0,
            max_frames: None,
        }
    }

    /// Limit total frames (useful for finite tests).
    pub fn with_max_frames(mut self, max: u64) -> Self {
        self.max_frames = Some(max);
        self
    }

    /// Frames produced so far.
    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    /// PTS that will be assigned to the next frame.
    pub fn next_pts(&self) -> Duration {
        self.next_pts
    }

    fn render_bars(&self, frame_index: u64) -> Vec<u8> {
        // Seven vertical bars; slight phase shift by frame_index so pixels change.
        const BARS: [[u8; 3]; 7] = [
            [192, 192, 192], // white-ish
            [192, 192, 0],   // yellow
            [0, 192, 192],   // cyan
            [0, 192, 0],     // green
            [192, 0, 192],   // magenta
            [192, 0, 0],     // red
            [0, 0, 192],     // blue
        ];
        let w = self.width as usize;
        let h = self.height as usize;
        let mut data = vec![0u8; w * h * 3];
        let shift = (frame_index as usize) % 7;
        for y in 0..h {
            for x in 0..w {
                let bar = (x * 7 / w + shift) % 7;
                let c = BARS[bar];
                let i = (y * w + x) * 3;
                data[i] = c[0];
                data[i + 1] = c[1];
                data[i + 2] = c[2];
            }
        }
        // Encode frame index in the first few pixels for glass-to-glass harnesses.
        if data.len() >= 8 {
            let idx = frame_index.to_le_bytes();
            data[0] = idx[0];
            data[1] = idx[1];
            data[2] = idx[2];
            data[3] = idx[3];
            data[4] = idx[4];
            data[5] = idx[5];
            data[6] = idx[6];
            data[7] = idx[7];
        }
        data
    }
}

impl VideoSource for SyntheticVideoBars {
    type Error = SyntheticError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        if let Some(max) = self.max_frames {
            if self.frames_emitted >= max {
                return Ok(None);
            }
        }
        let pts = self.next_pts;
        let data = self.render_bars(self.frames_emitted);
        let frame = VideoFrame {
            pts_host_mono: pts,
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgb24,
            data,
        };
        self.frames_emitted += 1;
        self.next_pts = pts + self.frame_interval;
        Ok(Some(frame))
    }
}

/// Continuous sine-tone audio generator (default 440 Hz, 48 kHz, 10 ms packets).
#[derive(Debug, Clone)]
pub struct SyntheticAudioTone {
    sample_rate: u32,
    channels: u16,
    frequency_hz: f64,
    amplitude: f64,
    packet_frames: u32,
    /// Absolute sample index of the next packet's first sample.
    sample_index: u64,
    /// Host-mono PTS of sample index 0.
    start_pts: Duration,
    packets_emitted: u64,
    max_packets: Option<u64>,
}

impl SyntheticAudioTone {
    /// Create a tone generator.
    ///
    /// - `sample_rate`: e.g. 48_000
    /// - `channels`: 1 or 2
    /// - `frequency_hz`: tone frequency
    /// - `packet_ms`: packet duration in milliseconds (typically 10)
    /// - `start_pts`: host-mono of the first sample
    pub fn new(
        sample_rate: u32,
        channels: u16,
        frequency_hz: f64,
        packet_ms: u32,
        start_pts: Duration,
    ) -> Self {
        assert!(sample_rate > 0 && channels > 0 && packet_ms > 0);
        let packet_frames = sample_rate * packet_ms / 1000;
        assert!(packet_frames > 0);
        Self {
            sample_rate,
            channels,
            frequency_hz,
            amplitude: 0.2,
            packet_frames,
            sample_index: 0,
            start_pts,
            packets_emitted: 0,
            max_packets: None,
        }
    }

    /// 48 kHz mono 440 Hz tone in 10 ms packets — RemoteLink test default.
    pub fn default_a440(start_pts: Duration) -> Self {
        Self::new(48_000, 1, 440.0, 10, start_pts)
    }

    /// Limit total packets.
    pub fn with_max_packets(mut self, max: u64) -> Self {
        self.max_packets = Some(max);
        self
    }

    /// Packets produced so far.
    pub fn packets_emitted(&self) -> u64 {
        self.packets_emitted
    }

    fn pts_for_sample_index(&self, index: u64) -> Duration {
        let ns = index.saturating_mul(1_000_000_000) / u64::from(self.sample_rate);
        self.start_pts + Duration::from_nanos(ns)
    }
}

impl AudioSource for SyntheticAudioTone {
    type Error = SyntheticError;

    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error> {
        if let Some(max) = self.max_packets {
            if self.packets_emitted >= max {
                return Ok(None);
            }
        }
        let pts = self.pts_for_sample_index(self.sample_index);
        let n = self.packet_frames as usize;
        let ch = self.channels as usize;
        let mut pcm = Vec::with_capacity(n * ch);
        for i in 0..n {
            let t = (self.sample_index + i as u64) as f64 / f64::from(self.sample_rate);
            let s = (self.amplitude * (TAU * self.frequency_hz * t).sin() * i16::MAX as f64) as i16;
            for _ in 0..ch {
                pcm.push(s);
            }
        }
        self.sample_index += u64::from(self.packet_frames);
        self.packets_emitted += 1;
        Ok(Some(AudioFrame::from_s16(
            pts,
            self.sample_rate,
            self.channels,
            pcm,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp_clock::RtpEpoch;

    #[test]
    fn video_bars_produce_well_formed_frames() {
        let mut src = SyntheticVideoBars::new(64, 36, 30, Duration::ZERO).with_max_frames(3);
        for _ in 0..3 {
            let f = src.next_frame().unwrap().unwrap();
            assert!(f.is_well_formed());
            assert_eq!(f.format, PixelFormat::Rgb24);
        }
        assert!(src.next_frame().unwrap().is_none());
    }

    #[test]
    fn video_pts_advances_by_frame_interval() {
        let mut src = SyntheticVideoBars::new(16, 16, 50, Duration::from_millis(100));
        let f0 = src.next_frame().unwrap().unwrap();
        let f1 = src.next_frame().unwrap().unwrap();
        assert_eq!(f0.pts_host_mono, Duration::from_millis(100));
        // 50 fps → 20 ms
        assert_eq!(f1.pts_host_mono, Duration::from_millis(120));
    }

    #[test]
    fn audio_tone_10ms_packets_at_48k() {
        let mut src = SyntheticAudioTone::default_a440(Duration::ZERO).with_max_packets(2);
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.frame_count(), 480);
        assert_eq!(f.duration(), Duration::from_millis(10));
        let f2 = src.next_frame().unwrap().unwrap();
        assert_eq!(f2.pts_host_mono, Duration::from_millis(10));
    }

    #[test]
    fn synthetic_av_share_start_pts_for_rtp_epoch() {
        let t0 = Duration::from_millis(500);
        let mut video = SyntheticVideoBars::new(32, 18, 30, t0);
        let mut audio = SyntheticAudioTone::default_a440(t0);
        let vf = video.next_frame().unwrap().unwrap();
        let af = audio.next_frame().unwrap().unwrap();
        assert_eq!(vf.pts_host_mono, t0);
        assert_eq!(af.pts_host_mono, t0);

        let epoch = RtpEpoch::new(t0);
        assert_eq!(epoch.video_ts(vf.pts_host_mono), 0);
        assert_eq!(epoch.audio_ts(af.pts_host_mono), 0);
    }
}
