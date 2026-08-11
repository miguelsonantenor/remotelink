//! Video decode hooks: mock MH264 + synthetic fallback (no real H.264 on windows-gnu).

use std::time::Duration;

use remotelink_media::{H264Decoder, MockH264Decoder, PixelFormat, VideoFrame};
use remotelink_net::VideoNalu;

/// A decoded (or synthetic stand-in) video frame ready for present / tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    /// Presentation timestamp from the NALU / capture clock.
    pub pts_host_mono: Duration,
    /// True when the source AU was marked keyframe.
    pub keyframe: bool,
    /// Pixel frame (RGB/BGRA from mock decoder, or synthetic RGB).
    pub frame: VideoFrame,
    /// Original encoded payload length (for stats / tests).
    pub encoded_len: usize,
    /// True when pixels came from the mock MH264 decoder (not synthetic fill).
    pub from_mock_h264: bool,
}

/// Hook invoked for each inbound video NALU before present.
///
/// Toolkit shells and unit tests install their own implementation. The default
/// [`MockOrSyntheticDecoder`] decodes MH264 NALUs from
/// [`remotelink_media::MockSoftwareEncoder`] and falls back to placeholder RGB
/// for non-mock bitstreams so CI stays free of libavcodec / hardware decode.
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
            from_mock_h264: false,
        })
    }
}

/// Decodes mock MH264 Annex-B access units into RGB/BGRA frames.
///
/// Only accepts bitstreams from [`remotelink_media::MockSoftwareEncoder`].
/// Non-mock NALUs return `None` (caller may fall back).
#[derive(Debug, Default)]
pub struct MockH264VideoDecoder {
    inner: MockH264Decoder,
    recorded_nalus: Vec<VideoNalu>,
}

impl MockH264VideoDecoder {
    /// Create a mock H.264 decoder hook.
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames successfully decoded.
    pub fn frames_decoded(&self) -> u64 {
        self.inner.frames_decoded()
    }

    /// Recorded encoded NALUs.
    pub fn recorded_nalus(&self) -> &[VideoNalu] {
        &self.recorded_nalus
    }
}

impl VideoDecodeHook for MockH264VideoDecoder {
    fn decode(&mut self, nalu: &VideoNalu) -> Option<DecodedVideoFrame> {
        if nalu.data.is_empty() {
            return None;
        }
        if !MockH264Decoder::is_mock_bitstream(&nalu.data) {
            return None;
        }
        self.recorded_nalus.push(nalu.clone());
        match self
            .inner
            .decode(&nalu.data, nalu.pts_host_mono, nalu.keyframe)
        {
            Ok(Some(frame)) => Some(DecodedVideoFrame {
                pts_host_mono: nalu.pts_host_mono,
                keyframe: nalu.keyframe,
                encoded_len: nalu.data.len(),
                frame,
                from_mock_h264: true,
            }),
            Ok(None) => None,
            Err(_) => None,
        }
    }
}

/// Prefer mock MH264 decode; fall back to synthetic RGB for non-mock NALUs.
///
/// This is the default session decoder so host mock-encode paths and legacy
/// synthetic NALU tests both work without configuration.
#[derive(Debug)]
pub struct MockOrSyntheticDecoder {
    mock: MockH264VideoDecoder,
    synthetic: SyntheticVideoDecoder,
    /// When true, synthetic fallback is disabled (strict mock-only).
    mock_only: bool,
}

impl Default for MockOrSyntheticDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MockOrSyntheticDecoder {
    /// Create with synthetic fallback enabled (64×36).
    pub fn new() -> Self {
        Self {
            mock: MockH264VideoDecoder::new(),
            synthetic: SyntheticVideoDecoder::default(),
            mock_only: false,
        }
    }

    /// Synthetic fallback geometry.
    pub fn with_synthetic_size(width: u32, height: u32) -> Self {
        Self {
            mock: MockH264VideoDecoder::new(),
            synthetic: SyntheticVideoDecoder::new(width, height),
            mock_only: false,
        }
    }

    /// Disable synthetic fallback (return `None` for non-MH264).
    pub fn mock_only(mut self) -> Self {
        self.mock_only = true;
        self
    }

    /// Mock path frame count.
    pub fn mock_frames(&self) -> u64 {
        self.mock.frames_decoded()
    }

    /// Synthetic path frame count.
    pub fn synthetic_frames(&self) -> u64 {
        self.synthetic.frames_decoded()
    }
}

impl VideoDecodeHook for MockOrSyntheticDecoder {
    fn decode(&mut self, nalu: &VideoNalu) -> Option<DecodedVideoFrame> {
        if let Some(decoded) = self.mock.decode(nalu) {
            return Some(decoded);
        }
        if self.mock_only {
            return None;
        }
        self.synthetic.decode(nalu)
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
    use remotelink_media::{
        H264Encoder, H264EncoderConfig, MockSoftwareEncoder, PixelFormat, VideoFrame,
    };
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
        assert!(!out.from_mock_h264);
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

    #[test]
    fn mock_h264_decode_nalu_to_frame() {
        let mut enc = MockSoftwareEncoder::new(&H264EncoderConfig {
            width: 8,
            height: 4,
            fps: 30,
            target_bitrate_bps: 1_000_000,
        });
        let src = VideoFrame {
            pts_host_mono: Duration::from_millis(10),
            width: 8,
            height: 4,
            format: PixelFormat::Rgb24,
            data: vec![0x55; 8 * 4 * 3],
        };
        let au = enc.encode(&src, true).unwrap();
        let nalu = VideoNalu {
            pts_host_mono: au.pts_host_mono,
            rtp_ts: Some(900),
            keyframe: au.keyframe,
            format: NaluFormat::AnnexB,
            data: au.data,
        };
        let mut dec = MockH264VideoDecoder::new();
        let out = dec.decode(&nalu).expect("mock frame");
        assert!(out.from_mock_h264);
        assert_eq!(out.frame.width, 8);
        assert_eq!(out.frame.height, 4);
        assert!(out.frame.is_well_formed());
        assert_eq!(dec.frames_decoded(), 1);
    }

    #[test]
    fn auto_decoder_prefers_mock_then_synthetic() {
        let mut dec = MockOrSyntheticDecoder::new();
        // Non-mock → synthetic.
        let synthetic_nalu = VideoNalu {
            pts_host_mono: Duration::from_millis(1),
            rtp_ts: None,
            keyframe: true,
            format: NaluFormat::AnnexB,
            data: vec![0, 0, 0, 1, 0x65, 0x01],
        };
        let s = dec.decode(&synthetic_nalu).unwrap();
        assert!(!s.from_mock_h264);
        assert_eq!(dec.synthetic_frames(), 1);

        let mut enc = MockSoftwareEncoder::new(&H264EncoderConfig::default());
        let src = VideoFrame {
            pts_host_mono: Duration::from_millis(2),
            width: 4,
            height: 4,
            format: PixelFormat::Rgb24,
            data: vec![1u8; 4 * 4 * 3],
        };
        let au = enc.encode(&src, true).unwrap();
        let mock_nalu = VideoNalu {
            pts_host_mono: au.pts_host_mono,
            rtp_ts: Some(0),
            keyframe: true,
            format: NaluFormat::AnnexB,
            data: au.data,
        };
        let m = dec.decode(&mock_nalu).unwrap();
        assert!(m.from_mock_h264);
        assert_eq!(dec.mock_frames(), 1);
    }
}
