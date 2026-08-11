//! Video capture errors, config, and frame-sink delivery for Linux host.
//!
//! Sources implement [`remotelink_media::VideoSource`] (shared frame types + PTS).

use std::fmt;

use remotelink_media::source::{VideoFrame, VideoSource};
use thiserror::Error;

/// Errors from display capture open / frame acquisition.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// Capture is not available on this platform or build.
    #[error("display capture unsupported on this platform/build")]
    Unsupported,
    /// Requested display / node index is out of range.
    #[error("display index {0} not found")]
    DisplayNotFound(u32),
    /// PipeWire / portal session setup failed.
    #[error("capture device error: {0}")]
    Device(String),
    /// Portal permission denied or session lost (user cancelled, compositor change).
    #[error("pipewire / portal session lost or denied")]
    SessionLost,
    /// Timed wait or temporary idle condition surfaced as hard failure.
    #[error("capture timeout: {0}")]
    Timeout(String),
    /// Frame sink rejected a frame.
    #[error("frame sink error: {0}")]
    Sink(String),
    /// Invalid configuration.
    #[error("invalid capture config: {0}")]
    InvalidConfig(&'static str),
    /// Other capture failure.
    #[error("capture error: {0}")]
    Other(String),
}

/// Trait sink that receives timestamped video frames from a capture loop.
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

/// Capture open configuration (v1: single display / node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Display / monitor index (v1 always `0` = primary / first portal stream).
    pub display_index: u32,
    /// Preferred capture width (0 = portal/native default).
    pub width: u32,
    /// Preferred capture height (0 = portal/native default).
    pub height: u32,
    /// Target frame interval hint in milliseconds (mock uses this for PTS steps).
    pub frame_interval_ms: u32,
    /// Host-mono PTS origin for the first mock frame (media epoch `t0` candidate).
    pub start_pts_ms: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            display_index: 0,
            width: 320,
            height: 180,
            frame_interval_ms: 33,
            start_pts_ms: 0,
        }
    }
}

impl CaptureConfig {
    /// CI / unit-test defaults with explicit mock-friendly geometry.
    pub fn synthetic() -> Self {
        Self::default()
    }
}

/// Which capture backend to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureBackend {
    /// Platform default: PipeWire portal on Linux when linked; else [`CaptureError::Unsupported`].
    #[default]
    Platform,
    /// Deterministic synthetic frames (unit tests / CI without a compositor).
    Mock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::mock::MockVideoSource;
    use remotelink_media::PixelFormat;
    use std::time::Duration;

    #[test]
    fn pump_delivers_mock_frames_to_sink() {
        let mut src = MockVideoSource::new(4, 4, PixelFormat::Rgb24, Duration::from_millis(10))
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
    }

    #[test]
    fn collecting_sink_starts_empty() {
        let sink = CollectingFrameSink::new();
        assert!(sink.is_empty());
        assert_eq!(sink.len(), 0);
    }

    #[test]
    fn capture_error_messages() {
        assert!(CaptureError::Unsupported
            .to_string()
            .contains("unsupported"));
        assert!(CaptureError::SessionLost.to_string().contains("session"));
        assert!(CaptureError::DisplayNotFound(2).to_string().contains('2'));
    }
}
