//! Pure-Rust mock H.264 software encoder (CI-safe, no GPU / no openh264).
//!
//! Emits Annex-B access units with stub SPS/PPS on keyframes and a slice NAL
//! that embeds frame geometry + a compact pixel sample. **Not** a real H.264
//! bitstream — viewers must use a matching mock decoder or treat this as a
//! PeerTransport wiring payload only.
//!
//! Magic prefix inside the primary slice: `MH264` (analogous to media mock Opus
//! `MOPU`) so harnesses never confuse this with real encoder output.

use super::h264::{
    EncodeError, EncodedAccessUnit, EncoderBackendKind, EncoderConfig, H264Encoder, NaluFormat,
    DEFAULT_TARGET_BITRATE_BPS,
};
use crate::capture::{PixelFormat, VideoFrame};

/// Mock Annex-B software encoder.
///
/// # Bitstream layout (keyframe)
///
/// ```text
/// 00 00 00 01 67 <SPS stub: width, height, fps, bitrate>
/// 00 00 00 01 68 <PPS stub: 1 byte id>
/// 00 00 00 01 65 <IDR: magic MH264 + meta + pixel sample>
/// ```
///
/// Non-keyframes omit SPS/PPS and use NAL type `0x41` (non-IDR coded slice).
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

/// Magic for mock slice payloads.
pub const MOCK_SLICE_MAGIC: &[u8; 5] = b"MH264";

impl MockSoftwareEncoder {
    /// Create a mock encoder from config (never fails; zero dims taken from frames).
    pub fn new(config: &EncoderConfig) -> Self {
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
            keyframe_pending: true, // first frame is always a keyframe
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

    /// Whether a keyframe was requested and not yet emitted.
    pub fn keyframe_pending(&self) -> bool {
        self.keyframe_pending
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

    fn validate_frame(frame: &VideoFrame) -> Result<(), EncodeError> {
        if !frame.is_well_formed() {
            return Err(EncodeError::InvalidFrame(
                "frame buffer does not match dimensions/stride".into(),
            ));
        }
        match frame.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 | PixelFormat::Rgb24 => Ok(()),
        }
    }

    fn build_sps(&self, width: u32, height: u32) -> Vec<u8> {
        // NAL header 0x67 = type 7 (SPS), forbidden_zero=0, nal_ref_idc=3.
        let mut nal = vec![0x67];
        nal.extend_from_slice(MOCK_SLICE_MAGIC);
        nal.extend_from_slice(&width.to_le_bytes());
        nal.extend_from_slice(&height.to_le_bytes());
        nal.extend_from_slice(&self.fps.to_le_bytes());
        nal.extend_from_slice(&self.target_bitrate_bps.to_le_bytes());
        annexb_nal(&nal)
    }

    fn build_pps() -> Vec<u8> {
        // NAL header 0x68 = type 8 (PPS).
        annexb_nal(&[0x68, 0x00])
    }

    fn build_slice(&self, preview: &PreviewSample, keyframe: bool, index: u64) -> Vec<u8> {
        // 0x65 = IDR (type 5), 0x41 = non-IDR (type 1), nal_ref_idc=2.
        let nal_type: u8 = if keyframe { 0x65 } else { 0x41 };
        let mut nal = vec![nal_type];
        nal.extend_from_slice(MOCK_SLICE_MAGIC);
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

/// Max RGB/BGRA preview stored in a mock AU. Larger captures are downscaled.
const MAX_PREVIEW_WIDTH: u32 = 1280;
const MAX_PREVIEW_HEIGHT: u32 = 720;

struct PreviewSample {
    width: u32,
    height: u32,
    bpp: u8,
    pixels: Vec<u8>,
}

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
    PreviewSample {
        width: dw,
        height: dh,
        bpp: 3,
        pixels: box_downscale_rgb(frame, dw, dh, src_bpp),
    }
}

fn pixel_rgb(frame: &VideoFrame, sx: usize, sy: usize, src_bpp: usize) -> [u32; 3] {
    let stride = frame.stride.max(frame.width * src_bpp as u32) as usize;
    let si = sy * stride + sx * src_bpp;
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
    }
}

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
            pixels[di] = acc[0].checked_div(n).unwrap_or(0) as u8;
            pixels[di + 1] = acc[1].checked_div(n).unwrap_or(0) as u8;
            pixels[di + 2] = acc[2].checked_div(n).unwrap_or(0) as u8;
        }
    }
    pixels
}

impl H264Encoder for MockSoftwareEncoder {
    type Error = EncodeError;

    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, Self::Error> {
        Self::validate_frame(frame)?;

        // Learn geometry from first frame when config left width/height at 0.
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
        data.extend_from_slice(&self.build_slice(&preview, keyframe, self.frames_encoded));

        self.frames_encoded = self.frames_encoded.saturating_add(1);
        if keyframe {
            self.keyframe_pending = false;
        }

        Ok(EncodedAccessUnit {
            pts_host_mono: frame.pts_host_mono,
            keyframe,
            format: NaluFormat::AnnexB,
            data,
            target_bitrate_bps: self.target_bitrate_bps,
        })
    }

    fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    fn set_target_bitrate_bps(&mut self, bps: u32) {
        self.target_bitrate_bps = if bps == 0 {
            DEFAULT_TARGET_BITRATE_BPS
        } else {
            bps
        };
    }

    fn target_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn backend_kind(&self) -> EncoderBackendKind {
        EncoderBackendKind::SoftwareMock
    }
}

fn annexb_nal(nal_without_start_code: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + nal_without_start_code.len());
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal_without_start_code);
    out
}

/// Parse mock SPS width/height from an Annex-B AU (test helper).
#[cfg(test)]
pub fn find_mock_sps_geometry(data: &[u8]) -> Option<(u32, u32)> {
    // Look for start code + 0x67 + magic.
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i..i + 4] == [0, 0, 0, 1] && i + 5 < data.len() && data[i + 4] == 0x67 {
            let body = &data[i + 5..];
            if body.len() >= 5 + 8 && &body[..5] == MOCK_SLICE_MAGIC {
                let w = u32::from_le_bytes(body[5..9].try_into().ok()?);
                let h = u32::from_le_bytes(body[9..13].try_into().ok()?);
                return Some((w, h));
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixelFormat;
    use std::time::Duration;

    fn frame(w: u32, h: u32, pts_ms: u64) -> VideoFrame {
        let bpp = 4u32;
        let data = vec![0x11; (w * h * bpp) as usize];
        VideoFrame::packed(
            Duration::from_millis(pts_ms),
            w,
            h,
            PixelFormat::Bgra8,
            data,
        )
    }

    #[test]
    fn keyframe_has_sps_pps_and_idr() {
        let mut enc = MockSoftwareEncoder::new(&EncoderConfig {
            force_software: true,
            width: 16,
            height: 9,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            disable_hw_encode: true,
        });
        let au = enc.encode(&frame(16, 9, 0), false).unwrap();
        assert!(au.keyframe);
        assert_eq!(au.format, NaluFormat::AnnexB);
        assert_eq!(find_mock_sps_geometry(&au.data), Some((16, 9)));
        // SPS, PPS, IDR start codes
        let starts = au
            .data
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| w[4])
            .collect::<Vec<_>>();
        assert!(starts.contains(&0x67), "SPS missing: {starts:?}");
        assert!(starts.contains(&0x68), "PPS missing: {starts:?}");
        assert!(starts.contains(&0x65), "IDR missing: {starts:?}");
        assert!(au.data.windows(5).any(|w| w == MOCK_SLICE_MAGIC));
    }

    #[test]
    fn delta_frame_omits_parameter_sets() {
        let mut enc = MockSoftwareEncoder::new(&EncoderConfig {
            force_software: true,
            ..EncoderConfig::default()
        })
        .with_keyframe_interval(0);
        let _ = enc.encode(&frame(8, 8, 0), true).unwrap();
        let au = enc.encode(&frame(8, 8, 33), false).unwrap();
        assert!(!au.keyframe);
        let types: Vec<u8> = au
            .data
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| w[4])
            .collect();
        assert_eq!(types, vec![0x41]);
    }

    #[test]
    fn request_keyframe_and_bitrate_feedback() {
        let mut enc = MockSoftwareEncoder::new(&EncoderConfig {
            force_software: true,
            ..EncoderConfig::default()
        })
        .with_keyframe_interval(0);
        let _ = enc.encode(&frame(4, 4, 0), true).unwrap();
        enc.set_target_bitrate_bps(1_500_000);
        assert_eq!(enc.target_bitrate_bps(), 1_500_000);
        enc.request_keyframe();
        assert!(enc.keyframe_pending());
        let au = enc.encode(&frame(4, 4, 66), false).unwrap();
        assert!(au.keyframe);
        assert_eq!(au.target_bitrate_bps, 1_500_000);
        assert!(!enc.keyframe_pending());
    }

    #[test]
    fn rejects_malformed_frame() {
        let mut enc = MockSoftwareEncoder::new(&EncoderConfig::default());
        let bad = VideoFrame {
            pts_host_mono: Duration::ZERO,
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Bgra8,
            data: vec![0; 8], // too short
        };
        assert!(matches!(
            enc.encode(&bad, false),
            Err(EncodeError::InvalidFrame(_))
        ));
    }

    #[test]
    fn accepts_rgb24() {
        let mut enc = MockSoftwareEncoder::new(&EncoderConfig {
            force_software: true,
            ..EncoderConfig::default()
        });
        let f = VideoFrame::packed(Duration::ZERO, 2, 2, PixelFormat::Rgb24, vec![0; 2 * 2 * 3]);
        let au = enc.encode(&f, true).unwrap();
        assert!(au.keyframe);
        assert_eq!(au.pts_host_mono, Duration::ZERO);
    }
}
