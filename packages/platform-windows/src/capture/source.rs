//! Video capture traits, errors, and frame-sink delivery.

use std::fmt;

use thiserror::Error;

use super::frame::VideoFrame;

/// Errors from display capture open / frame acquisition.
#[derive(Debug, Error, Clone)]
pub enum CaptureError {
    /// Capture is not available on this platform or build.
    #[error("display capture unsupported on this platform")]
    Unsupported,
    /// Requested display index is out of range.
    #[error("display index {0} not found")]
    DisplayNotFound(u32),
    /// DXGI / D3D device or duplication setup failed.
    #[error("capture device error: {0}")]
    Device(String),
    /// Desktop duplication access lost (mode change, secure desktop, etc.).
    ///
    /// Caller should drop and re-open the capturer. On UAC / Winlogon secure
    /// desktop this may recur until the host user returns to an interactive
    /// desktop (v1 known gap — see module docs).
    #[error("desktop duplication access lost (secure desktop or mode change)")]
    AccessLost,
    /// Timed wait or temporary idle condition surfaced as hard failure.
    #[error("capture timeout: {0}")]
    Timeout(String),
    /// Frame sink rejected a frame.
    #[error("frame sink error: {0}")]
    Sink(String),
    /// Other capture failure.
    #[error("capture error: {0}")]
    Other(String),
}

/// Trait for video capture sources (DXGI Desktop Duplication or mock).
///
/// Compatible in spirit with `remotelink_media::VideoSource`: pull next frame
/// with host-mono PTS, or `Ok(None)` when temporarily idle (no desktop update).
pub trait VideoSource {
    /// Error type produced by this source.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Pull the next frame, or `Ok(None)` if the source is temporarily idle
    /// (DXGI timeout / no dirty rects) or mock end-of-stream.
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error>;
}

/// Trait sink that receives timestamped video frames from a capture loop.
///
/// Session agent encode / PeerTransport paths will implement this (PR 16b).
pub trait FrameSink {
    /// Error type produced by this sink.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Accept one captured frame (borrowed; sink may clone if it needs ownership).
    fn on_frame(&mut self, frame: &VideoFrame) -> Result<(), Self::Error>;
}

/// Collecting sink for unit tests and smoke wiring.
#[derive(Debug, Default)]
pub struct CollectingFrameSink {
    /// Frames received in order.
    pub frames: Vec<VideoFrame>,
}

impl CollectingFrameSink {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of frames received.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether no frames have been received.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl FrameSink for CollectingFrameSink {
    type Error = CaptureError;

    fn on_frame(&mut self, frame: &VideoFrame) -> Result<(), Self::Error> {
        self.frames.push(frame.clone());
        Ok(())
    }
}

/// Error from [`pump_frame`] when source or sink fails.
#[derive(Debug)]
pub enum PumpError<Se, Ke> {
    /// Capture source failed.
    Source(Se),
    /// Frame sink failed.
    Sink(Ke),
}

impl<Se: fmt::Display, Ke: fmt::Display> fmt::Display for PumpError<Se, Ke> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PumpError::Source(e) => write!(f, "source: {e}"),
            PumpError::Sink(e) => write!(f, "sink: {e}"),
        }
    }
}

impl<Se, Ke> std::error::Error for PumpError<Se, Ke>
where
    Se: std::error::Error + 'static,
    Ke: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PumpError::Source(e) => Some(e),
            PumpError::Sink(e) => Some(e),
        }
    }
}

/// Pull one frame from `source` and, if present, deliver it to `sink`.
///
/// Returns `Ok(true)` if a frame was delivered, `Ok(false)` if the source was idle.
pub fn pump_frame<Src, Snk>(
    source: &mut Src,
    sink: &mut Snk,
) -> Result<bool, PumpError<Src::Error, Snk::Error>>
where
    Src: VideoSource,
    Snk: FrameSink,
{
    match source.next_frame() {
        Err(e) => Err(PumpError::Source(e)),
        Ok(None) => Ok(false),
        Ok(Some(frame)) => {
            sink.on_frame(&frame).map_err(PumpError::Sink)?;
            Ok(true)
        }
    }
}

/// Capture open configuration (v1: single display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Zero-based output index (v1 always `0` = primary).
    pub display_index: u32,
    /// `AcquireNextFrame` timeout in milliseconds (DXGI). Mock ignores this.
    pub timeout_ms: u32,
    /// Mock source width (0 = default 320). Ignored by DXGI.
    pub mock_width: u32,
    /// Mock source height (0 = default 180). Ignored by DXGI.
    pub mock_height: u32,
    /// Mock source FPS (0 = ~30). Ignored by DXGI.
    pub mock_fps: u32,
    /// Mock first-frame PTS in milliseconds.
    pub mock_start_pts_ms: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            display_index: 0,
            timeout_ms: 16,
            mock_width: 0,
            mock_height: 0,
            mock_fps: 0,
            mock_start_pts_ms: 0,
        }
    }
}

/// Which capture backend to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureBackend {
    /// Platform default: DXGI Desktop Duplication on Windows; [`CaptureError::Unsupported`] elsewhere.
    #[default]
    Platform,
    /// Deterministic synthetic frames (unit tests / CI without a real desktop).
    Mock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::frame::{host_mono_now, PixelFormat, VideoFrame};
    use crate::capture::mock::MockVideoSource;
    use std::time::Duration;

    #[test]
    fn pump_delivers_mock_frames_to_sink() {
        let mut src = MockVideoSource::new(4, 4, PixelFormat::Bgra8, Duration::from_millis(10))
            .with_max_frames(3);
        let mut sink = CollectingFrameSink::new();
        let mut delivered = 0;
        while pump_frame(&mut src, &mut sink).unwrap() {
            delivered += 1;
        }
        assert_eq!(delivered, 3);
        assert_eq!(sink.len(), 3);
        assert!(sink.frames.iter().all(|f| f.is_well_formed()));
        assert!(sink.frames[0].pts_host_mono < sink.frames[1].pts_host_mono);
        assert!(sink.frames[1].pts_host_mono < sink.frames[2].pts_host_mono);
    }

    #[test]
    fn collecting_sink_starts_empty() {
        let sink = CollectingFrameSink::new();
        assert!(sink.is_empty());
        assert_eq!(sink.len(), 0);
        let _ = host_mono_now();
        let _ = VideoFrame::packed(Duration::ZERO, 1, 1, PixelFormat::Rgb24, vec![0; 3]);
    }
}
