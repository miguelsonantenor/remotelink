//! H.264 encoder trait, config, and factory.

use std::fmt;
use std::time::Duration;

use thiserror::Error;

use super::hardware::HardwareEncoderStub;
use super::software::MockSoftwareEncoder;
use crate::capture::VideoFrame;

/// Default target bitrate when policy does not specify one (4 Mbps).
pub const DEFAULT_TARGET_BITRATE_BPS: u32 = 4_000_000;

/// How NAL units are framed in [`EncodedAccessUnit::data`].
///
/// Mirrors the net crate packaging so the host can copy the field into
/// `remotelink_net::VideoNalu` without remapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NaluFormat {
    /// Annex-B: `00 00 00 01` / `00 00 01` start codes.
    AnnexB,
    /// AVCC: 4-byte big-endian length prefixes.
    Avcc,
}

/// One encoded H.264 access unit (one or more NAL units) ready for PeerTransport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    /// Host-monotonic PTS copied from the source frame.
    pub pts_host_mono: Duration,
    /// True if this AU contains an IDR (and typically SPS/PPS on the mock path).
    pub keyframe: bool,
    /// Bitstream packaging (v1 mock always [`NaluFormat::AnnexB`]).
    pub format: NaluFormat,
    /// Encoded bytes.
    pub data: Vec<u8>,
    /// Encoder's notion of target bitrate at encode time (for stats / tests).
    pub target_bitrate_bps: u32,
}

/// Which concrete backend produced NALUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderBackendKind {
    /// Pure-Rust mock / software-fallback path (CI-safe).
    SoftwareMock,
    /// Hardware encoder (NVENC / QSV / AMF) — stub in this PR.
    Hardware,
}

impl EncoderBackendKind {
    /// Stable label for logs / stats.
    pub fn as_str(self) -> &'static str {
        match self {
            EncoderBackendKind::SoftwareMock => "software_mock",
            EncoderBackendKind::Hardware => "hardware",
        }
    }
}

/// Errors from H.264 encode open / encode.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// Frame geometry or pixel format cannot be encoded.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    /// Encoder was closed or never opened.
    #[error("encoder closed")]
    Closed,
    /// Hardware path is not available on this machine / build.
    #[error("hardware H.264 encoder unavailable: {0}")]
    HardwareUnavailable(String),
    /// Bitrate / config rejected.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// Other encode failure.
    #[error("encode error: {0}")]
    Other(String),
}

/// Configuration for opening an H.264 encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Nominal encode width (0 = take from first frame).
    pub width: u32,
    /// Nominal encode height (0 = take from first frame).
    pub height: u32,
    /// Target frame rate (used for mock SPS timing fields / future real encoders).
    pub fps: u32,
    /// Initial target bitrate in bits per second.
    pub target_bitrate_bps: u32,
    /// When true, never attempt hardware encode (feature flag / policy).
    pub disable_hw_encode: bool,
    /// When true, force the software/mock path even if HW might be available.
    ///
    /// Equivalent intent to [`Self::disable_hw_encode`]; both are honored by
    /// [`open_encoder`]. Kept separate so policy (`disable_hw_encode`) and
    /// local debug (`force_software`) can be set independently.
    pub force_software: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            fps: 30,
            target_bitrate_bps: DEFAULT_TARGET_BITRATE_BPS,
            disable_hw_encode: false,
            force_software: false,
        }
    }
}

impl EncoderConfig {
    /// True when the software/mock path must be used.
    pub fn prefer_software(&self) -> bool {
        self.disable_hw_encode || self.force_software
    }
}

/// Encode BGRA/RGB frames to H.264 Annex-B (or AVCC) NAL access units.
pub trait H264Encoder {
    /// Error type produced by this encoder.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Encode one raw frame.
    ///
    /// When `force_keyframe` is true, or an earlier [`Self::request_keyframe`]
    /// is pending, emit an IDR (and parameter sets on the mock path).
    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, Self::Error>;

    /// Request that the next encode emit a keyframe (PLI / FIR).
    fn request_keyframe(&mut self);

    /// Adapt encode bitrate from GCC / transport-cc feedback (in-process).
    fn set_target_bitrate_bps(&mut self, bps: u32);

    /// Current target bitrate.
    fn target_bitrate_bps(&self) -> u32;

    /// Which backend this instance is.
    fn backend_kind(&self) -> EncoderBackendKind;
}

/// Type-erased encoder used by the session manager.
#[derive(Debug)]
pub enum AnyH264Encoder {
    /// Software / mock path.
    Software(MockSoftwareEncoder),
    /// Hardware path (stub until real SDK wiring).
    Hardware(HardwareEncoderStub),
}

impl H264Encoder for AnyH264Encoder {
    type Error = EncodeError;

    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, Self::Error> {
        match self {
            AnyH264Encoder::Software(e) => e.encode(frame, force_keyframe),
            AnyH264Encoder::Hardware(e) => e.encode(frame, force_keyframe),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            AnyH264Encoder::Software(e) => e.request_keyframe(),
            AnyH264Encoder::Hardware(e) => e.request_keyframe(),
        }
    }

    fn set_target_bitrate_bps(&mut self, bps: u32) {
        match self {
            AnyH264Encoder::Software(e) => e.set_target_bitrate_bps(bps),
            AnyH264Encoder::Hardware(e) => e.set_target_bitrate_bps(bps),
        }
    }

    fn target_bitrate_bps(&self) -> u32 {
        match self {
            AnyH264Encoder::Software(e) => e.target_bitrate_bps(),
            AnyH264Encoder::Hardware(e) => e.target_bitrate_bps(),
        }
    }

    fn backend_kind(&self) -> EncoderBackendKind {
        match self {
            AnyH264Encoder::Software(e) => e.backend_kind(),
            AnyH264Encoder::Hardware(e) => e.backend_kind(),
        }
    }
}

/// Open an H.264 encoder according to `config`.
///
/// Selection order:
/// 1. If [`EncoderConfig::prefer_software`] → [`MockSoftwareEncoder`]
/// 2. Else try [`HardwareEncoderStub::try_open`]
/// 3. On HW failure → fall back to [`MockSoftwareEncoder`]
///
/// Today step 2 always fails (documented stub), so production CI always lands
/// on the mock software path unless a future PR implements real HW open.
pub fn open_encoder(config: &EncoderConfig) -> Result<AnyH264Encoder, EncodeError> {
    if config.fps == 0 {
        return Err(EncodeError::InvalidConfig("fps must be > 0".into()));
    }
    if config.prefer_software() {
        return Ok(AnyH264Encoder::Software(MockSoftwareEncoder::new(config)));
    }
    match HardwareEncoderStub::try_open(config) {
        Ok(hw) => Ok(AnyH264Encoder::Hardware(hw)),
        Err(EncodeError::HardwareUnavailable(_)) => {
            Ok(AnyH264Encoder::Software(MockSoftwareEncoder::new(config)))
        }
        Err(e) => Err(e),
    }
}

impl fmt::Display for NaluFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NaluFormat::AnnexB => write!(f, "annexb"),
            NaluFormat::Avcc => write!(f, "avcc"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{PixelFormat, VideoFrame};
    use std::time::Duration;

    fn tiny_bgra() -> VideoFrame {
        VideoFrame::packed(
            Duration::from_millis(10),
            4,
            4,
            PixelFormat::Bgra8,
            vec![0x80; 4 * 4 * 4],
        )
    }

    #[test]
    fn prefer_software_flags() {
        let mut c = EncoderConfig::default();
        assert!(!c.prefer_software());
        c.disable_hw_encode = true;
        assert!(c.prefer_software());
        c.disable_hw_encode = false;
        c.force_software = true;
        assert!(c.prefer_software());
    }

    #[test]
    fn open_encoder_force_software_is_mock() {
        let enc = open_encoder(&EncoderConfig {
            force_software: true,
            ..EncoderConfig::default()
        })
        .unwrap();
        assert_eq!(enc.backend_kind(), EncoderBackendKind::SoftwareMock);
        let au = match enc {
            AnyH264Encoder::Software(mut e) => e.encode(&tiny_bgra(), true).unwrap(),
            AnyH264Encoder::Hardware(_) => panic!("expected software"),
        };
        assert!(au.keyframe);
        assert_eq!(au.format, NaluFormat::AnnexB);
        assert!(au.data.windows(4).any(|w| w == [0, 0, 0, 1]));
    }

    #[test]
    fn open_encoder_default_falls_back_to_software() {
        // Hardware stub always unavailable → software fallback.
        let enc = open_encoder(&EncoderConfig::default()).unwrap();
        assert_eq!(enc.backend_kind(), EncoderBackendKind::SoftwareMock);
    }

    #[test]
    fn open_encoder_rejects_zero_fps() {
        let err = open_encoder(&EncoderConfig {
            fps: 0,
            force_software: true,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(matches!(err, EncodeError::InvalidConfig(_)));
    }
}
