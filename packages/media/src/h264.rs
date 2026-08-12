//! Mock H.264 encode/decode (pure Rust, CI-safe).
//!
//! Real libavcodec / openh264 is **not** linked on windows-gnu. This module
//! provides a software mock that embeds geometry + a compact pixel sample in
//! Annex-B NAL units so host → viewer paths can be tested without a GPU.
//!
//! Magic prefix inside the primary slice / SPS body: `MH264` (analogous to
//! mock Opus `MOPU`). Bitstream layout matches the host
//! `MockSoftwareEncoder` (platform-windows encode path) so viewer decode
//! accepts NALUs produced on either side.
//!
//! # Bitstream layout (keyframe)
//!
//! ```text
//! 00 00 00 01 67 <SPS stub: MH264 + width, height, fps, bitrate>
//! 00 00 00 01 68 <PPS stub: 1 byte id>
//! 00 00 00 01 65 <IDR: MH264 + meta + pixel sample>
//! ```
//!
//! Non-keyframes omit SPS/PPS and use NAL type `0x41` (non-IDR coded slice).

use std::time::Duration;

use crate::source::{PixelFormat, VideoFrame};

/// Default target bitrate when policy does not specify one (4 Mbps).
pub const DEFAULT_TARGET_BITRATE_BPS: u32 = 4_000_000;

/// Magic for mock slice / SPS payloads (`MH264`).
pub const MOCK_H264_MAGIC: &[u8; 5] = b"MH264";

/// How NAL units are framed in encoded access units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum H264NaluFormat {
    /// Annex-B: `00 00 00 01` / `00 00 01` start codes.
    AnnexB,
    /// AVCC: 4-byte big-endian length prefixes.
    Avcc,
}

/// One encoded H.264 access unit ready for PeerTransport / tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    /// Host-monotonic PTS copied from the source frame.
    pub pts_host_mono: Duration,
    /// True if this AU contains an IDR (and typically SPS/PPS on the mock path).
    pub keyframe: bool,
    /// Bitstream packaging (v1 mock always [`H264NaluFormat::AnnexB`]).
    pub format: H264NaluFormat,
    /// Encoded bytes.
    pub data: Vec<u8>,
    /// Encoder's notion of target bitrate at encode time.
    pub target_bitrate_bps: u32,
}

/// Configuration for opening a mock H.264 encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264EncoderConfig {
    /// Nominal encode width (0 = take from first frame).
    pub width: u32,
    /// Nominal encode height (0 = take from first frame).
    pub height: u32,
    /// Target frame rate (stored in mock SPS).
    pub fps: u32,
    /// Initial target bitrate in bits per second.
    pub target_bitrate_bps: u32,
}

impl Default for H264EncoderConfig {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            fps: 30,
            target_bitrate_bps: DEFAULT_TARGET_BITRATE_BPS,
        }
    }
}

/// Errors from mock H.264 encode / decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H264Error {
    /// Frame geometry or pixel format cannot be encoded.
    InvalidFrame(&'static str),
    /// Encoded payload is not a mock MH264 bitstream (or is corrupt).
    InvalidBitstream(&'static str),
    /// Unsupported configuration.
    Unsupported(&'static str),
}

impl std::fmt::Display for H264Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            H264Error::InvalidFrame(m) => write!(f, "invalid frame: {m}"),
            H264Error::InvalidBitstream(m) => write!(f, "invalid bitstream: {m}"),
            H264Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for H264Error {}

/// Encode raw frames to mock Annex-B access units.
pub trait H264Encoder {
    /// Encode one raw frame. When `force_keyframe` is true, emit an IDR (+ SPS/PPS).
    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, H264Error>;

    /// Request that the next encode emit a keyframe.
    fn request_keyframe(&mut self);
}

/// Decode mock Annex-B access units to RGB/BGRA frames.
pub trait H264Decoder {
    /// Decode one access unit into a presentable frame.
    ///
    /// Returns `Ok(None)` only when the AU is empty. Invalid mock bitstreams
    /// return [`H264Error`].
    fn decode(
        &mut self,
        data: &[u8],
        pts_host_mono: Duration,
        keyframe: bool,
    ) -> Result<Option<VideoFrame>, H264Error>;
}

/// Pure-Rust mock software encoder (CI-safe, no GPU / no openh264).
#[derive(Debug, Clone)]
pub struct MockSoftwareEncoder {
    width: u32,
    height: u32,
    fps: u32,
    target_bitrate_bps: u32,
    frames_encoded: u64,
    keyframe_pending: bool,
    /// Force a keyframe every N frames (0 = only on request / first frame).
    keyframe_interval: u64,
}

impl MockSoftwareEncoder {
    /// Create a mock encoder from config.
    pub fn new(config: &H264EncoderConfig) -> Self {
        Self {
            width: config.width,
            height: config.height,
            fps: config.fps.max(1),
            target_bitrate_bps: if config.target_bitrate_bps == 0 {
                DEFAULT_TARGET_BITRATE_BPS
            } else {
                config.target_bitrate_bps
            },
            frames_encoded: 0,
            keyframe_pending: true,
            keyframe_interval: 30,
        }
    }

    /// Override automatic keyframe interval (0 = only explicit / first).
    pub fn with_keyframe_interval(mut self, interval: u64) -> Self {
        self.keyframe_interval = interval;
        self
    }

    /// Frames successfully encoded so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }

    /// Current target bitrate.
    pub fn target_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    /// Adapt target bitrate (GCC stub).
    pub fn set_target_bitrate_bps(&mut self, bps: u32) {
        self.target_bitrate_bps = if bps == 0 {
            DEFAULT_TARGET_BITRATE_BPS
        } else {
            bps
        };
    }

    fn should_keyframe(&self, force: bool) -> bool {
        if force || self.keyframe_pending {
            return true;
        }
        if self.frames_encoded == 0 {
            return true;
        }
        if self.keyframe_interval > 0 && self.frames_encoded.is_multiple_of(self.keyframe_interval)
        {
            return true;
        }
        false
    }

    fn validate_frame(frame: &VideoFrame) -> Result<(), H264Error> {
        if !frame.is_well_formed() {
            return Err(H264Error::InvalidFrame(
                "frame buffer does not match dimensions",
            ));
        }
        match frame.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 | PixelFormat::Rgb24 => Ok(()),
            PixelFormat::Gray8 => Err(H264Error::Unsupported("Gray8 not supported by mock H.264")),
        }
    }

    fn build_sps(&self, width: u32, height: u32) -> Vec<u8> {
        let mut nal = vec![0x67];
        nal.extend_from_slice(MOCK_H264_MAGIC);
        nal.extend_from_slice(&width.to_le_bytes());
        nal.extend_from_slice(&height.to_le_bytes());
        nal.extend_from_slice(&self.fps.to_le_bytes());
        nal.extend_from_slice(&self.target_bitrate_bps.to_le_bytes());
        annexb_nal(&nal)
    }

    fn build_pps() -> Vec<u8> {
        annexb_nal(&[0x68, 0x00])
    }

    fn build_slice_from_preview(
        &self,
        preview: &PreviewSample,
        keyframe: bool,
        index: u64,
    ) -> Vec<u8> {
        let nal_type: u8 = if keyframe { 0x65 } else { 0x41 };
        let mut nal = vec![nal_type];
        nal.extend_from_slice(MOCK_H264_MAGIC);
        nal.extend_from_slice(&index.to_le_bytes());
        nal.extend_from_slice(&preview.width.to_le_bytes());
        nal.extend_from_slice(&preview.height.to_le_bytes());
        nal.push(preview.bpp);
        nal.extend_from_slice(&self.target_bitrate_bps.to_le_bytes());
        nal.extend_from_slice(&(preview.pixels.len() as u32).to_le_bytes());
        nal.extend_from_slice(&preview.pixels);
        annexb_nal(&nal)
    }
}

/// Max preview stored in a mock AU (RGB24). Larger captures are downscaled.
const MAX_PREVIEW_WIDTH: u32 = 1280;
const MAX_PREVIEW_HEIGHT: u32 = 720;

struct PreviewSample {
    width: u32,
    height: u32,
    bpp: u8,
    pixels: Vec<u8>,
}

/// Embed a full (or downscaled RGB24) pixel buffer so the viewer can reconstruct
/// a real picture. Small test frames are stored losslessly.
fn preview_pixels(frame: &VideoFrame) -> PreviewSample {
    let src_bpp = frame.format.bytes_per_pixel();
    if frame.width <= MAX_PREVIEW_WIDTH
        && frame.height <= MAX_PREVIEW_HEIGHT
        && !frame.data.is_empty()
    {
        return PreviewSample {
            width: frame.width,
            height: frame.height,
            bpp: src_bpp as u8,
            pixels: frame.data.clone(),
        };
    }
    let scale = (MAX_PREVIEW_WIDTH as f32 / frame.width.max(1) as f32)
        .min(MAX_PREVIEW_HEIGHT as f32 / frame.height.max(1) as f32)
        .min(1.0);
    let dw = ((frame.width as f32) * scale).round().max(1.0) as u32;
    let dh = ((frame.height as f32) * scale).round().max(1.0) as u32;
    let pixels = box_downscale_rgb(frame, dw, dh, src_bpp);
    PreviewSample {
        width: dw,
        height: dh,
        bpp: 3,
        pixels,
    }
}

fn pixel_rgb(frame: &VideoFrame, sx: usize, sy: usize, src_bpp: usize) -> [u32; 3] {
    let si = (sy * frame.width as usize + sx) * src_bpp;
    if si + src_bpp > frame.data.len() {
        return [0, 0, 0];
    }
    match frame.format {
        PixelFormat::Rgb24 | PixelFormat::Rgba8 => [
            frame.data[si] as u32,
            frame.data[si + 1] as u32,
            frame.data[si + 2] as u32,
        ],
        PixelFormat::Bgra8 => [
            frame.data[si + 2] as u32,
            frame.data[si + 1] as u32,
            frame.data[si] as u32,
        ],
        PixelFormat::Gray8 => {
            let g = frame.data[si] as u32;
            [g, g, g]
        }
    }
}

/// Average each destination pixel over its source rectangle (less blocky than nearest).
fn box_downscale_rgb(frame: &VideoFrame, dw: u32, dh: u32, src_bpp: usize) -> Vec<u8> {
    let sw = frame.width.max(1);
    let sh = frame.height.max(1);
    let mut pixels = vec![0u8; dw as usize * dh as usize * 3];
    for y in 0..dh {
        let sy0 = (y as u64 * sh as u64 / dh as u64) as usize;
        let sy1 = (((y as u64 + 1) * sh as u64 / dh as u64) as usize).max(sy0 + 1);
        for x in 0..dw {
            let sx0 = (x as u64 * sw as u64 / dw as u64) as usize;
            let sx1 = (((x as u64 + 1) * sw as u64 / dw as u64) as usize).max(sx0 + 1);
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for sy in sy0..sy1.min(sh as usize) {
                for sx in sx0..sx1.min(sw as usize) {
                    let p = pixel_rgb(frame, sx, sy, src_bpp);
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                    n += 1;
                }
            }
            let di = (y as usize * dw as usize + x as usize) * 3;
            if n > 0 {
                pixels[di] = (acc[0] / n) as u8;
                pixels[di + 1] = (acc[1] / n) as u8;
                pixels[di + 2] = (acc[2] / n) as u8;
            }
        }
    }
    pixels
}

impl H264Encoder for MockSoftwareEncoder {
    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, H264Error> {
        Self::validate_frame(frame)?;

        if self.width == 0 {
            self.width = frame.width;
        }
        if self.height == 0 {
            self.height = frame.height;
        }

        let keyframe = self.should_keyframe(force_keyframe);
        let preview = preview_pixels(frame);
        let mut data = Vec::with_capacity(256 + preview.pixels.len());
        if keyframe {
            data.extend_from_slice(&self.build_sps(preview.width, preview.height));
            data.extend_from_slice(&Self::build_pps());
        }
        data.extend_from_slice(&self.build_slice_from_preview(
            &preview,
            keyframe,
            self.frames_encoded,
        ));

        self.frames_encoded = self.frames_encoded.saturating_add(1);
        if keyframe {
            self.keyframe_pending = false;
        }

        Ok(EncodedAccessUnit {
            pts_host_mono: frame.pts_host_mono,
            keyframe,
            format: H264NaluFormat::AnnexB,
            data,
            target_bitrate_bps: self.target_bitrate_bps,
        })
    }

    fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }
}

/// Decoded mock H.264 metadata recovered from a slice NAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockSliceMeta {
    /// Frame index embedded by the encoder.
    pub index: u64,
    /// Width from slice meta.
    pub width: u32,
    /// Height from slice meta.
    pub height: u32,
    /// Bytes-per-pixel of the source format (3 or 4).
    pub bpp: u8,
    /// Target bitrate stamp from encoder.
    pub target_bitrate_bps: u32,
    /// Compact pixel sample (up to 64 bytes).
    pub sample: Vec<u8>,
    /// True when the slice NAL type is IDR (0x65).
    pub keyframe: bool,
}

/// Pure-Rust mock software decoder for [`MockSoftwareEncoder`] bitstreams.
///
/// Reconstructs a full RGB24 or BGRA frame by tiling the embedded pixel sample.
/// Non-MH264 Annex-B payloads return [`H264Error::InvalidBitstream`] so callers
/// can fall back to a synthetic decoder.
#[derive(Debug, Clone, Default)]
pub struct MockH264Decoder {
    frames_decoded: u64,
    /// Last SPS geometry (width, height) if observed.
    last_sps: Option<(u32, u32)>,
    /// Last FPS from SPS.
    last_fps: Option<u32>,
}

impl MockH264Decoder {
    /// Create a new mock decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames successfully produced.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Last SPS geometry, if any.
    pub fn last_sps_geometry(&self) -> Option<(u32, u32)> {
        self.last_sps
    }

    /// Parse mock slice metadata from an Annex-B AU without building pixels.
    pub fn parse_slice_meta(data: &[u8]) -> Result<MockSliceMeta, H264Error> {
        parse_mock_slice(data)
    }

    /// Returns true if `data` looks like a mock MH264 access unit.
    pub fn is_mock_bitstream(data: &[u8]) -> bool {
        find_magic_after_start_code(data).is_some()
    }

    fn reconstruct_frame(
        meta: &MockSliceMeta,
        pts: Duration,
        sps: Option<(u32, u32)>,
    ) -> Result<VideoFrame, H264Error> {
        let (width, height) = if meta.width > 0 && meta.height > 0 {
            (meta.width, meta.height)
        } else if let Some(g) = sps {
            g
        } else {
            return Err(H264Error::InvalidBitstream("missing geometry"));
        };
        if width == 0 || height == 0 {
            return Err(H264Error::InvalidBitstream("zero geometry"));
        }

        let format = match meta.bpp {
            3 => PixelFormat::Rgb24,
            4 => PixelFormat::Bgra8,
            _ => PixelFormat::Rgb24,
        };
        let bpp = format.bytes_per_pixel();
        let total = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(bpp);
        let mut data = vec![0u8; total];
        if !meta.sample.is_empty() {
            if meta.sample.len() == total {
                data.copy_from_slice(&meta.sample);
            } else {
                // Legacy compact sample: tile across the full frame.
                let sample = &meta.sample;
                let mut off = 0;
                while off < total {
                    let n = (total - off).min(sample.len());
                    data[off..off + n].copy_from_slice(&sample[..n]);
                    off += n;
                }
            }
        }
        Ok(VideoFrame {
            pts_host_mono: pts,
            width,
            height,
            format,
            data,
        })
    }
}

impl H264Decoder for MockH264Decoder {
    fn decode(
        &mut self,
        data: &[u8],
        pts_host_mono: Duration,
        _keyframe: bool,
    ) -> Result<Option<VideoFrame>, H264Error> {
        if data.is_empty() {
            return Ok(None);
        }
        if let Some((w, h, fps)) = find_mock_sps(data) {
            self.last_sps = Some((w, h));
            self.last_fps = Some(fps);
        }
        let meta = parse_mock_slice(data)?;
        let frame = Self::reconstruct_frame(&meta, pts_host_mono, self.last_sps)?;
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        Ok(Some(frame))
    }
}

fn annexb_nal(nal_without_start_code: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + nal_without_start_code.len());
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal_without_start_code);
    out
}

/// Locate Annex-B start codes; yield byte offset of the NAL header (first byte
/// after the start code). Does **not** bound NAL length by scanning for the next
/// start code — mock payloads may contain `00 00 00 01` patterns in the pixel
/// sample (host mock encoder does not apply RBSP emulation prevention).
fn find_nal_header_offsets(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            out.push(i + 4);
            i += 4;
            continue;
        }
        if data[i..].starts_with(&[0, 0, 1]) {
            out.push(i + 3);
            i += 3;
            continue;
        }
        i += 1;
    }
    out
}

fn find_magic_after_start_code(data: &[u8]) -> Option<()> {
    for &hdr in &find_nal_header_offsets(data) {
        if hdr + 1 + MOCK_H264_MAGIC.len() <= data.len()
            && &data[hdr + 1..hdr + 1 + MOCK_H264_MAGIC.len()] == MOCK_H264_MAGIC
        {
            return Some(());
        }
    }
    // Bare magic without start codes (defensive for unit tests).
    if data
        .windows(MOCK_H264_MAGIC.len())
        .any(|w| w == MOCK_H264_MAGIC)
    {
        return Some(());
    }
    None
}

fn find_mock_sps(data: &[u8]) -> Option<(u32, u32, u32)> {
    // SPS body is fixed-size: magic(5) + w(4) + h(4) + fps(4) + bitrate(4) = 21
    const SPS_BODY: usize = 5 + 4 + 4 + 4 + 4;
    for &hdr in &find_nal_header_offsets(data) {
        if hdr >= data.len() {
            continue;
        }
        let nal_type = data[hdr] & 0x1f;
        if nal_type != 7 {
            continue;
        }
        let body = hdr + 1;
        if body + SPS_BODY > data.len() {
            continue;
        }
        if &data[body..body + 5] != MOCK_H264_MAGIC {
            continue;
        }
        let w = u32::from_le_bytes(data[body + 5..body + 9].try_into().ok()?);
        let h = u32::from_le_bytes(data[body + 9..body + 13].try_into().ok()?);
        let fps = u32::from_le_bytes(data[body + 13..body + 17].try_into().ok()?);
        return Some((w, h, fps));
    }
    None
}

fn parse_mock_slice(data: &[u8]) -> Result<MockSliceMeta, H264Error> {
    // Fixed header after NAL type byte:
    // magic(5) + index(8) + w(4) + h(4) + bpp(1) + bitrate(4) + sample_len(4) = 30
    // then `sample_len` payload bytes. Do not bound by next start code — the
    // sample may legally contain Annex-B patterns.
    const HDR: usize = 5 + 8 + 4 + 4 + 1 + 4 + 4;
    for &hdr in &find_nal_header_offsets(data) {
        if hdr >= data.len() {
            continue;
        }
        let nal_type = data[hdr] & 0x1f;
        // IDR (5) or non-IDR slice (1)
        if nal_type != 5 && nal_type != 1 {
            continue;
        }
        let body = hdr + 1;
        if body + HDR > data.len() {
            continue;
        }
        if &data[body..body + 5] != MOCK_H264_MAGIC {
            // Slice NAL without our magic — skip (real H.264 or garbage).
            continue;
        }
        let index = u64::from_le_bytes(
            data[body + 5..body + 13]
                .try_into()
                .map_err(|_| H264Error::InvalidBitstream("index"))?,
        );
        let width = u32::from_le_bytes(
            data[body + 13..body + 17]
                .try_into()
                .map_err(|_| H264Error::InvalidBitstream("width"))?,
        );
        let height = u32::from_le_bytes(
            data[body + 17..body + 21]
                .try_into()
                .map_err(|_| H264Error::InvalidBitstream("height"))?,
        );
        let bpp = data[body + 21];
        let target_bitrate_bps = u32::from_le_bytes(
            data[body + 22..body + 26]
                .try_into()
                .map_err(|_| H264Error::InvalidBitstream("bitrate"))?,
        );
        let sample_len = u32::from_le_bytes(
            data[body + 26..body + 30]
                .try_into()
                .map_err(|_| H264Error::InvalidBitstream("sample_len"))?,
        ) as usize;
        let sample_off = body + HDR;
        if sample_off + sample_len > data.len() {
            return Err(H264Error::InvalidBitstream("sample truncated"));
        }
        let sample = data[sample_off..sample_off + sample_len].to_vec();
        return Ok(MockSliceMeta {
            index,
            width,
            height,
            bpp,
            target_bitrate_bps,
            sample,
            keyframe: nal_type == 5,
        });
    }
    Err(H264Error::InvalidBitstream(
        "no MH264 slice NAL in access unit",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_frame(w: u32, h: u32, pts_ms: u64, fill: u8) -> VideoFrame {
        let data = vec![fill; (w * h * 3) as usize];
        VideoFrame {
            pts_host_mono: Duration::from_millis(pts_ms),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data,
        }
    }

    #[test]
    fn mock_keyframe_roundtrip_rgb() {
        let mut enc = MockSoftwareEncoder::new(&H264EncoderConfig {
            width: 8,
            height: 4,
            fps: 30,
            target_bitrate_bps: 2_000_000,
        });
        let src = rgb_frame(8, 4, 33, 0xAB);
        let au = enc.encode(&src, true).unwrap();
        assert!(au.keyframe);
        assert!(MockH264Decoder::is_mock_bitstream(&au.data));
        assert!(au.data.windows(5).any(|w| w == MOCK_H264_MAGIC));

        let mut dec = MockH264Decoder::new();
        let out = dec
            .decode(&au.data, au.pts_host_mono, au.keyframe)
            .unwrap()
            .expect("frame");
        assert_eq!(out.width, 8);
        assert_eq!(out.height, 4);
        assert_eq!(out.pts_host_mono, Duration::from_millis(33));
        assert!(out.is_well_formed());
        // Sample was tiled; first bytes match source sample.
        assert_eq!(&out.data[..3], &src.data[..3]);
        assert_eq!(out.data, src.data);
        assert_eq!(dec.frames_decoded(), 1);
    }

    #[test]
    fn mock_full_frame_pixels_roundtrip() {
        let mut enc = MockSoftwareEncoder::new(&H264EncoderConfig {
            width: 8,
            height: 4,
            fps: 30,
            target_bitrate_bps: 1_000_000,
        });
        let mut src = rgb_frame(8, 4, 10, 0x20);
        src.data[20] = 0xDE;
        src.data[21] = 0xAD;
        src.data[22] = 0xBE;
        let au = enc.encode(&src, true).unwrap();
        let mut dec = MockH264Decoder::new();
        let out = dec
            .decode(&au.data, au.pts_host_mono, true)
            .unwrap()
            .expect("frame");
        assert_eq!(out.data[20], 0xDE);
        assert_eq!(out.data[21], 0xAD);
        assert_eq!(out.data[22], 0xBE);
        assert_eq!(out.data, src.data);
    }

    #[test]
    fn mock_delta_roundtrip_bgra() {
        let mut enc =
            MockSoftwareEncoder::new(&H264EncoderConfig::default()).with_keyframe_interval(0);
        let bpp = 4u32;
        let mut data = vec![0u8; (16 * 9 * bpp) as usize];
        data[0] = 1;
        data[1] = 2;
        data[2] = 3;
        data[3] = 255;
        let f0 = VideoFrame {
            pts_host_mono: Duration::ZERO,
            width: 16,
            height: 9,
            format: PixelFormat::Bgra8,
            data: data.clone(),
        };
        let au0 = enc.encode(&f0, true).unwrap();
        assert!(au0.keyframe);

        let f1 = VideoFrame {
            pts_host_mono: Duration::from_millis(33),
            width: 16,
            height: 9,
            format: PixelFormat::Bgra8,
            data,
        };
        let au1 = enc.encode(&f1, false).unwrap();
        assert!(!au1.keyframe);
        // Delta has no SPS (only slice).
        assert!(!au1
            .data
            .windows(2)
            .any(|w| w == [0x67, b'M'] || w == [0x00, 0x67]));

        let mut dec = MockH264Decoder::new();
        let _ = dec
            .decode(&au0.data, au0.pts_host_mono, true)
            .unwrap()
            .unwrap();
        let out = dec
            .decode(&au1.data, au1.pts_host_mono, false)
            .unwrap()
            .unwrap();
        assert_eq!(out.format, PixelFormat::Bgra8);
        assert_eq!(&out.data[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn reject_non_mock_nalu() {
        let mut dec = MockH264Decoder::new();
        // Annex-B start + random NAL without magic.
        let data = vec![0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33];
        let err = dec.decode(&data, Duration::ZERO, true).unwrap_err();
        assert!(matches!(err, H264Error::InvalidBitstream(_)));
        assert!(!MockH264Decoder::is_mock_bitstream(&data));
    }

    #[test]
    fn empty_au_returns_none() {
        let mut dec = MockH264Decoder::new();
        assert!(dec.decode(&[], Duration::ZERO, false).unwrap().is_none());
    }

    #[test]
    fn request_keyframe_forces_idr() {
        let mut enc =
            MockSoftwareEncoder::new(&H264EncoderConfig::default()).with_keyframe_interval(0);
        let f = rgb_frame(4, 4, 0, 0);
        assert!(enc.encode(&f, false).unwrap().keyframe); // first
        assert!(!enc.encode(&f, false).unwrap().keyframe);
        enc.request_keyframe();
        assert!(enc.encode(&f, false).unwrap().keyframe);
    }
}
