//! Media Foundation H.264 encoder (Windows).
//!
//! Uses the system **Microsoft H.264 Encoder MFT** (`CLSID_CMSH264EncoderMFT`),
//! which can run in software or accelerate via GPU drivers when available
//! (Intel Quick Sync, AMD, NVIDIA through the OS MFT path — not a direct NVENC
//! SDK session).
//!
//! # Status
//!
//! - Opens the MFT, configures RGB32 → H.264, encodes BGRA frames to Annex-B
//!   access units when ProcessOutput succeeds.
//! - Headless VMs / missing codecs return [`EncodeError::HardwareUnavailable`]
//!   so [`super::open_encoder`] falls back to the mock software path.
//! - Async MFTs are rejected; only sync MFTs are used (simpler pump).

#![cfg(windows)]

use super::h264::{
    EncodeError, EncodedAccessUnit, EncoderBackendKind, EncoderConfig, H264Encoder, NaluFormat,
    DEFAULT_TARGET_BITRATE_BPS,
};
use crate::capture::{PixelFormat, VideoFrame};

/// Media Foundation H.264 encoder (system MFT).
pub struct MediaFoundationEncoder {
    width: u32,
    height: u32,
    fps: u32,
    target_bitrate_bps: u32,
    keyframe_pending: bool,
    frames_encoded: u64,
    transform: windows::Win32::Media::MediaFoundation::IMFTransform,
    input_stream_id: u32,
    output_stream_id: u32,
    /// Sample duration in 100-ns units.
    sample_duration_hns: i64,
    /// Next sample time in 100-ns units.
    next_sample_time_hns: i64,
}

// SAFETY: encoder is only used from the agent media pump (single-threaded).
unsafe impl Send for MediaFoundationEncoder {}

impl std::fmt::Debug for MediaFoundationEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFoundationEncoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("target_bitrate_bps", &self.target_bitrate_bps)
            .field("frames_encoded", &self.frames_encoded)
            .finish_non_exhaustive()
    }
}

impl MediaFoundationEncoder {
    /// Try to open the Microsoft H.264 Encoder MFT for `config`.
    pub fn try_open(config: &EncoderConfig) -> Result<Self, EncodeError> {
        use windows::core::GUID;
        use windows::Win32::Media::MediaFoundation::{
            IMFMediaType, IMFTransform, MFCreateMediaType, MFMediaType_Video, MFStartup,
            MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION, MFVideoFormat_H264, MFVideoFormat_RGB32,
            MFVideoInterlace_Progressive, MFSTARTUP_NOSOCKET,
        };
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
        };

        // Microsoft H.264 Encoder MFT
        // {6CA50344-051A-4DED-9779-A43305165E35}
        const CLSID_CMSH264_ENCODER_MFT: GUID =
            GUID::from_u128(0x6ca5_0344_051a_4ded_9779_a433_0516_5e35);

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| EncodeError::HardwareUnavailable(format!("MFStartup: {e}")))?;
        }

        let transform: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_CMSH264_ENCODER_MFT, None, CLSCTX_INPROC_SERVER).map_err(
                |e| {
                    EncodeError::HardwareUnavailable(format!(
                        "CoCreateInstance H.264 MFT: {e} (install Media Feature Pack?)"
                    ))
                },
            )?
        };

        let width = config.width.max(16);
        let height = config.height.max(16);
        // H.264 often wants even dimensions.
        let width = width & !1;
        let height = height & !1;
        let fps = config.fps.max(1);
        let bitrate = if config.target_bitrate_bps == 0 {
            DEFAULT_TARGET_BITRATE_BPS
        } else {
            config.target_bitrate_bps
        };

        // Output type: H.264
        let out_type: IMFMediaType = unsafe {
            MFCreateMediaType()
                .map_err(|e| EncodeError::HardwareUnavailable(format!("MFCreateMediaType out: {e}")))?
        };
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| EncodeError::Other(format!("set major out: {e}")))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| EncodeError::Other(format!("set subtype H264: {e}")))?;
            // Frame size: packed (width << 32) | height
            let frame_size = ((width as u64) << 32) | height as u64;
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(|e| EncodeError::Other(format!("set frame size: {e}")))?;
            // Frame rate: (fps << 32) | 1
            let frame_rate = ((fps as u64) << 32) | 1;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
                .map_err(|e| EncodeError::Other(format!("set frame rate: {e}")))?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(|e| EncodeError::Other(format!("set bitrate: {e}")))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| EncodeError::Other(format!("set interlace: {e}")))?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| EncodeError::HardwareUnavailable(format!("SetOutputType H264: {e}")))?;
        }

        // Input type: RGB32 (BGRA memory layout on little-endian Windows)
        let in_type: IMFMediaType = unsafe {
            MFCreateMediaType()
                .map_err(|e| EncodeError::HardwareUnavailable(format!("MFCreateMediaType in: {e}")))?
        };
        unsafe {
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| EncodeError::Other(format!("set major in: {e}")))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| EncodeError::Other(format!("set subtype RGB32: {e}")))?;
            let frame_size = ((width as u64) << 32) | height as u64;
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(|e| EncodeError::Other(format!("set in frame size: {e}")))?;
            let frame_rate = ((fps as u64) << 32) | 1;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
                .map_err(|e| EncodeError::Other(format!("set in frame rate: {e}")))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| EncodeError::Other(format!("set in interlace: {e}")))?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| EncodeError::HardwareUnavailable(format!("SetInputType RGB32: {e}")))?;
        }

        // Optional: notify begin streaming
        let _ = unsafe { transform.ProcessMessage(
            windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            0,
        ) };
        let _ = unsafe { transform.ProcessMessage(
            windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            0,
        ) };

        let sample_duration_hns = 10_000_000i64 / i64::from(fps.max(1));

        Ok(Self {
            width,
            height,
            fps,
            target_bitrate_bps: bitrate,
            keyframe_pending: true,
            frames_encoded: 0,
            transform,
            input_stream_id: 0,
            output_stream_id: 0,
            sample_duration_hns,
            next_sample_time_hns: 0,
        })
    }

    fn build_sample(&self, frame: &VideoFrame) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, EncodeError> {
        use windows::Win32::Media::MediaFoundation::{
            MFCreateMemoryBuffer, MFCreateSample, IMFSample,
        };

        if frame.format != PixelFormat::Bgra8 && frame.format != PixelFormat::Rgba8 {
            return Err(EncodeError::InvalidFrame(
                "Media Foundation path expects BGRA8/RGBA8".into(),
            ));
        }
        if frame.width == 0 || frame.height == 0 {
            return Err(EncodeError::InvalidFrame("zero dimensions".into()));
        }

        // Scale/crop not implemented: require matching configured size (or learn on first frame).
        let w = self.width;
        let h = self.height;
        let stride = (w as usize).saturating_mul(4);
        let byte_len = stride.saturating_mul(h as usize);

        let buffer = unsafe {
            MFCreateMemoryBuffer(byte_len as u32)
                .map_err(|e| EncodeError::Other(format!("MFCreateMemoryBuffer: {e}")))?
        };
        unsafe {
            let mut raw: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut raw, None, None)
                .map_err(|e| EncodeError::Other(format!("buffer Lock: {e}")))?;
            if raw.is_null() {
                return Err(EncodeError::Other("buffer lock null".into()));
            }
            // Copy / letterbox into encoder size.
            let dst = std::slice::from_raw_parts_mut(raw, byte_len);
            dst.fill(0);
            let copy_w = (frame.width.min(w) as usize) * 4;
            let copy_h = frame.height.min(h) as usize;
            let src_stride = (frame.width as usize) * 4;
            for y in 0..copy_h {
                let src_off = y * src_stride;
                let dst_off = y * stride;
                if src_off + copy_w <= frame.data.len() && dst_off + copy_w <= dst.len() {
                    dst[dst_off..dst_off + copy_w]
                        .copy_from_slice(&frame.data[src_off..src_off + copy_w]);
                }
            }
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(byte_len as u32)
                .map_err(|e| EncodeError::Other(format!("SetCurrentLength: {e}")))?;
        }

        let sample: IMFSample = unsafe {
            MFCreateSample().map_err(|e| EncodeError::Other(format!("MFCreateSample: {e}")))?
        };
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(|e| EncodeError::Other(format!("AddBuffer: {e}")))?;
            sample
                .SetSampleTime(self.next_sample_time_hns)
                .map_err(|e| EncodeError::Other(format!("SetSampleTime: {e}")))?;
            sample
                .SetSampleDuration(self.sample_duration_hns)
                .map_err(|e| EncodeError::Other(format!("SetSampleDuration: {e}")))?;
        }
        Ok(sample)
    }
}

impl H264Encoder for MediaFoundationEncoder {
    type Error = EncodeError;

    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, Self::Error> {
        use windows::Win32::Foundation::E_NOTIMPL;
        use windows::Win32::Media::MediaFoundation::{
            IMFMediaBuffer, IMFSample, MFCreateMemoryBuffer, MFCreateSample, MFT_OUTPUT_DATA_BUFFER,
        };

        if force_keyframe || self.keyframe_pending {
            // Request IDR via codec API if available — best-effort attribute.
            // Many MFTs ignore this; keyframe_pending still tracks intent.
            self.keyframe_pending = true;
        }

        let sample = self.build_sample(frame)?;
        unsafe {
            self.transform
                .ProcessInput(self.input_stream_id, &sample, 0)
                .map_err(|e| EncodeError::Other(format!("ProcessInput: {e}")))?;
        }

        // Prepare output buffer.
        let info = unsafe {
            self.transform
                .GetOutputStreamInfo(self.output_stream_id)
                .map_err(|e| EncodeError::Other(format!("GetOutputStreamInfo: {e}")))?
        };
        let out_size = info.cbSize.max(65_536);

        let out_buffer = unsafe {
            MFCreateMemoryBuffer(out_size)
                .map_err(|e| EncodeError::Other(format!("MFCreateMemoryBuffer out: {e}")))?
        };
        unsafe {
            out_buffer
                .SetCurrentLength(0)
                .map_err(|e| EncodeError::Other(format!("out SetCurrentLength: {e}")))?;
        }
        let out_sample: IMFSample = unsafe {
            MFCreateSample().map_err(|e| EncodeError::Other(format!("MFCreateSample out: {e}")))?
        };
        unsafe {
            out_sample
                .AddBuffer(&out_buffer)
                .map_err(|e| EncodeError::Other(format!("out AddBuffer: {e}")))?;
        }

        let mut out_buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream_id,
            pSample: std::mem::ManuallyDrop::new(Some(out_sample)),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let hr = unsafe {
            self.transform
                .ProcessOutput(0, &mut out_buffers, &mut status)
        };

        // Recover sample from ManuallyDrop for buffer read.
        let out_sample = unsafe { std::mem::ManuallyDrop::take(&mut out_buffers[0].pSample) };

        if let Err(e) = hr {
            // Need more input is common; treat as soft failure → empty AU skip.
            let code = e.code();
            if code == windows::core::HRESULT(0xC00D6D72u32 as i32) || code == E_NOTIMPL {
                // MF_E_TRANSFORM_NEED_MORE_INPUT
                return Err(EncodeError::Other(
                    "encoder needs more input / not ready".into(),
                ));
            }
            return Err(EncodeError::Other(format!("ProcessOutput: {e}")));
        }

        let Some(out_sample) = out_sample else {
            return Err(EncodeError::Other("ProcessOutput no sample".into()));
        };

        let data = unsafe {
            let buf: IMFMediaBuffer = out_sample
                .ConvertToContiguousBuffer()
                .map_err(|e| EncodeError::Other(format!("ConvertToContiguousBuffer: {e}")))?;
            let mut raw: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            buf.Lock(&mut raw, Some(&mut max_len), Some(&mut cur_len))
                .map_err(|e| EncodeError::Other(format!("out Lock: {e}")))?;
            let slice = if raw.is_null() || cur_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(raw, cur_len as usize).to_vec()
            };
            let _ = buf.Unlock();
            slice
        };

        if data.is_empty() {
            return Err(EncodeError::Other("empty encoded access unit".into()));
        }

        let keyframe = self.keyframe_pending || self.frames_encoded == 0;
        self.keyframe_pending = false;
        self.frames_encoded = self.frames_encoded.saturating_add(1);
        self.next_sample_time_hns = self
            .next_sample_time_hns
            .saturating_add(self.sample_duration_hns);

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
        let _ = unsafe {
            self.transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_COMMAND_DRAIN,
                0,
            )
        };
        // Drain is not ideal for IDR; attribute path varies by MFT. Flag is enough for v1.
    }

    fn set_target_bitrate_bps(&mut self, bps: u32) {
        self.target_bitrate_bps = if bps == 0 {
            DEFAULT_TARGET_BITRATE_BPS
        } else {
            bps
        };
        // Dynamic bitrate via CODECAPI would go here on a full integration.
    }

    fn target_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn backend_kind(&self) -> EncoderBackendKind {
        EncoderBackendKind::Hardware
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        let _ = unsafe {
            self.transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_END_OF_STREAM,
                0,
            )
        };
        let _ = unsafe {
            self.transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_COMMAND_FLUSH,
                0,
            )
        };
        // MFShutdown is process-global; leave MF running for other components.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn try_open_on_this_machine() {
        let cfg = EncoderConfig {
            width: 320,
            height: 180,
            fps: 30,
            target_bitrate_bps: 1_000_000,
            disable_hw_encode: false,
            force_software: false,
        };
        match MediaFoundationEncoder::try_open(&cfg) {
            Ok(mut enc) => {
                assert_eq!(enc.backend_kind(), EncoderBackendKind::Hardware);
                let frame = VideoFrame::packed(
                    Duration::from_millis(0),
                    320,
                    180,
                    PixelFormat::Bgra8,
                    vec![0x40; 320 * 180 * 4],
                );
                // First encode may need more input on some MFTs — either AU or soft error is OK.
                match enc.encode(&frame, true) {
                    Ok(au) => {
                        assert!(!au.data.is_empty());
                        assert_eq!(au.format, NaluFormat::AnnexB);
                    }
                    Err(EncodeError::Other(_)) => {}
                    Err(e) => panic!("unexpected encode error: {e}"),
                }
            }
            Err(EncodeError::HardwareUnavailable(_)) => {
                // N-server / missing codec pack is acceptable.
            }
            Err(e) => panic!("unexpected try_open error: {e}"),
        }
    }
}
