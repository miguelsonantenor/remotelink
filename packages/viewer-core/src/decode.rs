//! Video decode hooks for synthetic / mock NALUs (no real H.264 on windows-gnu).

use std::time::Duration;

use remotelink_media::{PixelFormat, VideoFrame};
use remotelink_net::VideoNalu;

/// A decoded (or synthetic stand-in) video frame ready for present / tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    /// Presentation timestamp from the NALU / capture clock.
    pub pts_host_mono: Duration,
    /// True when the source AU was marked keyframe.
    pub keyframe: bool,
    /// Pixel frame (synthetic RGB when no real decoder is linked).
    pub frame: VideoFrame,
    /// Original encoded payload length (for stats / tests).
    pub encoded_len: usize,
}

/// Hook invoked for each inbound video NALU before present.
///
/// Toolkit shells and unit tests install their own implementation. The default
/// [`SyntheticVideoDecoder`] builds placeholder RGB frames so CI stays free of
/// libavcodec / hardware decode.
pub trait VideoDecodeHook: Send {
    /// Decode or synthesize a presentable frame from an encoded NALU.
    fn decode(&mut self, nalu: &VideoNalu) -> Option<DecodedVideoFrame>;
}

/// Records every NALU and emits a small synthetic RGB frame per AU.
///
/// Frame pixels encode the receive index in the first bytes (glass-to-glass
/// harness style, matching `SyntheticVideoBars`).
#[derive(Debug, Clone)]
pub struct SyntheticVideoDecoder {
    /// Width of synthesized frames.
    pub width: u32,
    /// Height of synthesized frames.
    pub height: u32,
    /// Frames successfully produced.
    frames_decoded: u64,
    /// Raw NALUs observed (bitstream record for tests).
    recorded_nalus: Vec<VideoNalu>,
}

impl Default for SyntheticVideoDecoder {
    fn default() -> Self {
        Self::new(64, 36)
    }
}

impl SyntheticVideoDecoder {
    /// Create a decoder that synthesizes `width`×`height` RGB24 frames.
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0);
        Self {
            width,
            height,
            frames_decoded: 0,
            recorded_nalus: Vec::new(),
        }
    }

    /// Number of frames produced.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Recorded encoded NALUs (clone).
    pub fn recorded_nalus(&self) -> &[VideoNalu] {
        &self.recorded_nalus
    }

    /// Drain recorded NALUs.
    pub fn take_recorded_nalus(&mut self) -> Vec<VideoNalu> {
        std::mem::take(&mut self.recorded_nalus)
    }

    fn synthesize(&self, nalu: &VideoNalu, index: u64) -> VideoFrame {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut data = vec![0u8; w * h * 3];
        // Keyframes: brighter green-ish; deltas: blue-ish. Index in first pixels.
        let (r, g, b) = if nalu.keyframe {
            (32u8, 200u8, 64u8)
        } else {
            (32u8, 64u8, 180u8)
        };
        for px in data.chunks_exact_mut(3) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
        }
        if data.len() >= 8 {
            let idx = index.to_le_bytes();
            data[0] = idx[0];
            data[1] = idx[1];
            data[2] = idx[2];
            data[3] = idx[3];
            data[4] = idx[4];
            data[5] = idx[5];
            data[6] = idx[6];
            data[7] = idx[7];
        }
        VideoFrame {
            pts_host_mono: nalu.pts_host_mono,
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgb24,
            data,
        }
    }
}

impl VideoDecodeHook for SyntheticVideoDecoder {
    fn decode(&mut self, nalu: &VideoNalu) -> Option<DecodedVideoFrame> {
        if nalu.data.is_empty() {
            return None;
        }
        self.recorded_nalus.push(nalu.clone());
        let index = self.frames_decoded;
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        let frame = self.synthesize(nalu, index);
        Some(DecodedVideoFrame {
            pts_host_mono: nalu.pts_host_mono,
            keyframe: nalu.keyframe,
            encoded_len: nalu.data.len(),
            frame,
        })
    }
}

/// Decode hook that only records NALUs (no pixel synthesis).
#[derive(Debug, Default, Clone)]
pub struct RecordingDecodeHook {
    /// Recorded NALUs.
    pub nalu: Vec<VideoNalu>,
}

impl VideoDecodeHook for RecordingDecodeHook {
    fn decode(&mut self, nalu: &VideoNalu) -> Option<DecodedVideoFrame> {
        self.nalu.push(nalu.clone());
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_net::NaluFormat;
    use std::time::Duration;

    #[test]
    fn synthetic_decoder_records_and_builds_frame() {
        let mut dec = SyntheticVideoDecoder::new(8, 4);
        let nalu = VideoNalu {
            pts_host_mono: Duration::from_millis(33),
            rtp_ts: Some(2970),
            keyframe: true,
            format: NaluFormat::AnnexB,
            data: vec![0, 0, 0, 1, 0x65],
        };
        let out = dec.decode(&nalu).expect("frame");
        assert!(out.keyframe);
        assert_eq!(out.frame.width, 8);
        assert!(out.frame.is_well_formed());
        assert_eq!(dec.frames_decoded(), 1);
        assert_eq!(dec.recorded_nalus().len(), 1);
    }

    #[test]
    fn empty_nalu_skipped() {
        let mut dec = SyntheticVideoDecoder::default();
        let nalu = VideoNalu {
            pts_host_mono: Duration::ZERO,
            rtp_ts: None,
            keyframe: false,
            format: NaluFormat::AnnexB,
            data: vec![],
        };
        assert!(dec.decode(&nalu).is_none());
        assert_eq!(dec.frames_decoded(), 0);
    }
}
