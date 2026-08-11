//! Deterministic mock video source for unit tests (no real desktop required).

use std::time::Duration;

use super::frame::{host_mono_now, PixelFormat, VideoFrame};
use super::source::{CaptureError, VideoSource};

/// Synthetic solid-color / gradient frames with advancing host-mono PTS.
///
/// Safe to use on all platforms; never touches DXGI or a GPU.
/// Constructors never panic: zero dimensions are rejected by [`Self::try_new`]
/// or clamped by [`Self::new`].
#[derive(Debug, Clone)]
pub struct MockVideoSource {
    width: u32,
    height: u32,
    format: PixelFormat,
    frame_interval: Duration,
    /// PTS of the next frame; when `None`, stamp with [`host_mono_now`] at emit.
    next_pts: Option<Duration>,
    use_wall_clock: bool,
    frames_emitted: u64,
    max_frames: Option<u64>,
    /// If set, the next [`VideoSource::next_frame`] returns this error once.
    next_error: Option<CaptureError>,
}

impl MockVideoSource {
    /// Create a mock source with fixed PTS steps from `Duration::ZERO`.
    ///
    /// Zero `width` / `height` are clamped to 1 (never panics). Prefer
    /// [`Self::try_new`] when invalid sizes should be errors.
    pub fn new(width: u32, height: u32, format: PixelFormat, frame_interval: Duration) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            format,
            frame_interval,
            next_pts: Some(Duration::ZERO),
            use_wall_clock: false,
            frames_emitted: 0,
            max_frames: None,
            next_error: None,
        }
    }

    /// Fallible constructor: rejects zero dimensions instead of clamping.
    pub fn try_new(
        width: u32,
        height: u32,
        format: PixelFormat,
        frame_interval: Duration,
    ) -> Result<Self, CaptureError> {
        if width == 0 || height == 0 {
            return Err(CaptureError::Other(
                "mock video source width and height must be > 0".into(),
            ));
        }
        Ok(Self::new(width, height, format, frame_interval))
    }

    /// Make the next [`VideoSource::next_frame`] call return `err` once (tests).
    pub fn with_next_error(mut self, err: CaptureError) -> Self {
        self.next_error = Some(err);
        self
    }

    /// Stamp each frame with [`host_mono_now`] at emit time (real-time mock).
    pub fn with_wall_clock(mut self) -> Self {
        self.use_wall_clock = true;
        self.next_pts = None;
        self
    }

    /// Set the PTS of the first frame (ignored when using wall clock).
    pub fn with_start_pts(mut self, start: Duration) -> Self {
        if !self.use_wall_clock {
            self.next_pts = Some(start);
        }
        self
    }

    /// Limit total frames; further [`VideoSource::next_frame`] calls return `Ok(None)`.
    pub fn with_max_frames(mut self, max: u64) -> Self {
        self.max_frames = Some(max);
        self
    }

    /// Frames produced so far.
    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    fn render(&self, index: u64) -> Vec<u8> {
        let bpp = self.format.bytes_per_pixel();
        let w = self.width as usize;
        let h = self.height as usize;
        let stride = w * bpp;
        let mut data = vec![0u8; stride * h];
        let phase = (index % 256) as u8;
        for y in 0..h {
            for x in 0..w {
                let i = y * stride + x * bpp;
                match self.format {
                    PixelFormat::Bgra8 => {
                        data[i] = phase; // B
                        data[i + 1] = (x as u8).wrapping_add(phase); // G
                        data[i + 2] = (y as u8).wrapping_add(phase); // R
                        data[i + 3] = 255; // A
                    }
                    PixelFormat::Rgba8 => {
                        data[i] = (y as u8).wrapping_add(phase); // R
                        data[i + 1] = (x as u8).wrapping_add(phase); // G
                        data[i + 2] = phase; // B
                        data[i + 3] = 255; // A
                    }
                    PixelFormat::Rgb24 => {
                        data[i] = (y as u8).wrapping_add(phase);
                        data[i + 1] = (x as u8).wrapping_add(phase);
                        data[i + 2] = phase;
                    }
                }
            }
        }
        // Encode frame index in the first 8 bytes for harnesses (when buffer allows).
        if data.len() >= 8 {
            let idx = index.to_le_bytes();
            data[..8].copy_from_slice(&idx);
        }
        data
    }
}

impl VideoSource for MockVideoSource {
    type Error = CaptureError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        if let Some(err) = self.next_error.take() {
            return Err(err);
        }
        if let Some(max) = self.max_frames {
            if self.frames_emitted >= max {
                return Ok(None);
            }
        }
        let pts = if self.use_wall_clock {
            host_mono_now()
        } else {
            let pts = self.next_pts.unwrap_or(Duration::ZERO);
            self.next_pts = Some(pts + self.frame_interval);
            pts
        };
        let data = self.render(self.frames_emitted);
        let frame = VideoFrame::packed(pts, self.width, self.height, self.format, data);
        self.frames_emitted += 1;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_emits_finite_bgra_frames() {
        let mut src = MockVideoSource::new(8, 4, PixelFormat::Bgra8, Duration::from_millis(33))
            .with_max_frames(2);
        let f0 = src.next_frame().unwrap().unwrap();
        let f1 = src.next_frame().unwrap().unwrap();
        assert!(src.next_frame().unwrap().is_none());
        assert!(f0.is_well_formed());
        assert!(f1.is_well_formed());
        assert_eq!(f0.format, PixelFormat::Bgra8);
        assert_eq!(f0.width, 8);
        assert_eq!(f0.height, 4);
        assert_eq!(f0.pts_host_mono, Duration::ZERO);
        assert_eq!(f1.pts_host_mono, Duration::from_millis(33));
        assert_eq!(src.frames_emitted(), 2);
    }

    #[test]
    fn mock_start_pts() {
        let mut src = MockVideoSource::new(2, 2, PixelFormat::Rgb24, Duration::from_millis(5))
            .with_start_pts(Duration::from_millis(100))
            .with_max_frames(1);
        let f = src.next_frame().unwrap().unwrap();
        assert_eq!(f.pts_host_mono, Duration::from_millis(100));
        assert_eq!(f.format, PixelFormat::Rgb24);
        assert!(f.is_well_formed());
    }

    #[test]
    fn mock_new_clamps_zero_dims_without_panic() {
        let src = MockVideoSource::new(0, 0, PixelFormat::Bgra8, Duration::from_millis(1));
        assert_eq!(src.width, 1);
        assert_eq!(src.height, 1);
    }

    #[test]
    fn mock_try_new_rejects_zero_dims() {
        assert!(matches!(
            MockVideoSource::try_new(0, 4, PixelFormat::Bgra8, Duration::from_millis(1)),
            Err(CaptureError::Other(_))
        ));
        assert!(
            MockVideoSource::try_new(4, 4, PixelFormat::Bgra8, Duration::from_millis(1)).is_ok()
        );
    }

    #[test]
    fn mock_next_error_fires_once() {
        let mut src = MockVideoSource::new(2, 2, PixelFormat::Bgra8, Duration::from_millis(1))
            .with_next_error(CaptureError::AccessLost)
            .with_max_frames(2);
        assert!(matches!(src.next_frame(), Err(CaptureError::AccessLost)));
        assert!(src.next_frame().unwrap().is_some());
    }
}
