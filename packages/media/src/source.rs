//! Capture source traits and timestamped frame types.
//!
//! Frames carry host-monotonic presentation timestamps (`pts_host_mono`) measured
//! from an arbitrary origin (typically process start or a session clock). RTP
//! mapping uses a shared session epoch via [`crate::rtp_clock::RtpEpoch`].

use std::time::Duration;

/// Pixel layout of a video frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit RGB, tightly packed (3 bytes/pixel).
    Rgb24,
    /// 8-bit RGBA (4 bytes/pixel).
    Rgba8,
    /// 8-bit BGRA, tightly packed (4 bytes/pixel). DXGI / mock H.264 native.
    Bgra8,
    /// Single-plane 8-bit grayscale.
    Gray8,
}

impl PixelFormat {
    /// Bytes per pixel for tightly packed formats.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb24 => 3,
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
            PixelFormat::Gray8 => 1,
        }
    }
}

/// PCM sample representation for audio frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Interleaved signed 16-bit little-endian PCM.
    S16Le,
    /// Interleaved IEEE float32 little-endian PCM.
    F32Le,
}

/// A captured (or synthetic) video frame with host-monotonic PTS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Host monotonic time of capture / intended presentation origin.
    pub pts_host_mono: Duration,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel layout of [`Self::data`].
    pub format: PixelFormat,
    /// Tightly packed pixel data, row-major.
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// Expected byte length for the frame dimensions and format.
    pub fn expected_len(&self) -> usize {
        (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(self.format.bytes_per_pixel())
    }

    /// Returns true if `data` matches dimensions and format.
    pub fn is_well_formed(&self) -> bool {
        self.width > 0 && self.height > 0 && self.data.len() == self.expected_len()
    }
}

/// A captured (or synthetic) audio frame with host-monotonic PTS.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    /// Host monotonic time of the first sample in this frame.
    pub pts_host_mono: Duration,
    /// Sample rate in Hz (RemoteLink default: 48_000).
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u16,
    /// Declared sample format for [`Self::pcm_s16`] (v1 path is always S16).
    pub format: SampleFormat,
    /// Interleaved PCM samples as i16 (authoritative sample buffer for v1).
    pub pcm_s16: Vec<i16>,
}

impl AudioFrame {
    /// Number of frames (one sample per channel counts as one frame).
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.pcm_s16.len() / self.channels as usize
    }

    /// Duration covered by this PCM block.
    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        let frames = self.frame_count() as u64;
        Duration::from_nanos(frames.saturating_mul(1_000_000_000) / u64::from(self.sample_rate))
    }

    /// Create an i16 PCM audio frame.
    pub fn from_s16(
        pts_host_mono: Duration,
        sample_rate: u32,
        channels: u16,
        pcm_s16: Vec<i16>,
    ) -> Self {
        Self {
            pts_host_mono,
            sample_rate,
            channels,
            format: SampleFormat::S16Le,
            pcm_s16,
        }
    }
}

/// Trait for video capture sources (real DXGI or synthetic).
pub trait VideoSource {
    /// Error type produced by this source.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Pull the next frame, or `Ok(None)` if the source is temporarily idle /
    /// end-of-stream for synthetic finite generators.
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error>;
}

/// Trait for audio capture sources (real WASAPI loopback or synthetic).
pub trait AudioSource {
    /// Error type produced by this source.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Pull the next audio packet, or `Ok(None)` if idle / EOS.
    fn next_frame(&mut self) -> Result<Option<AudioFrame>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_frame_well_formed() {
        let f = VideoFrame {
            pts_host_mono: Duration::from_millis(1),
            width: 2,
            height: 2,
            format: PixelFormat::Rgb24,
            data: vec![0; 2 * 2 * 3],
        };
        assert!(f.is_well_formed());
        assert_eq!(f.expected_len(), 12);
    }

    #[test]
    fn audio_frame_duration_10ms_48k() {
        // 10 ms @ 48 kHz mono = 480 samples
        let f = AudioFrame::from_s16(Duration::ZERO, 48_000, 1, vec![0i16; 480]);
        assert_eq!(f.frame_count(), 480);
        assert_eq!(f.duration(), Duration::from_millis(10));
    }
}
