//! Hardware H.264 encoder **stub** (NVENC / Quick Sync / AMF).
//!
//! # Status (PR 16b)
//!
//! Real GPU encode SDKs are **not** linked. [`HardwareEncoderStub::try_open`]
//! always returns [`EncodeError::HardwareUnavailable`] so:
//!
//! - CI / headless / windows-gnu builds never require a GPU
//! - [`super::open_encoder`] falls back to [`super::MockSoftwareEncoder`]
//! - The type and method surface document the future HW path
//!
//! # Planned real path (follow-up)
//!
//! 1. Probe adapters (DXGI) for NVENC / QSV / AMF capability.
//! 2. Create encoder session at configured width/height/fps/bitrate.
//! 3. Submit BGRA (or NV12) frames from DXGI textures zero-copy when possible.
//! 4. Pull Annex-B or AVCC AUs; map PTS; honor PLI/FIR/GCC like the mock.
//!
//! Until then, any code that holds a `HardwareEncoderStub` was constructed via
//! test-only helpers; production always uses software fallback.

use super::h264::{
    EncodeError, EncodedAccessUnit, EncoderBackendKind, EncoderConfig, H264Encoder,
    DEFAULT_TARGET_BITRATE_BPS,
};
use crate::capture::VideoFrame;

/// Placeholder hardware encoder. Cannot encode until a real backend is wired.
#[derive(Debug, Clone)]
pub struct HardwareEncoderStub {
    target_bitrate_bps: u32,
    keyframe_pending: bool,
    /// Why open would fail / why this instance is inert.
    reason: String,
}

impl HardwareEncoderStub {
    /// Attempt to open a hardware encoder for `config`.
    ///
    /// **Always** returns [`EncodeError::HardwareUnavailable`] in this PR.
    pub fn try_open(config: &EncoderConfig) -> Result<Self, EncodeError> {
        let _ = config;
        Err(EncodeError::HardwareUnavailable(
            "HW H.264 (NVENC/QSV/AMF) not linked in this build; use software path \
             (disable_hw_encode / force_software) or await HW encoder PR"
                .into(),
        ))
    }

    /// Test-only: construct an inert HW stub that errors on encode.
    ///
    /// Production code must use [`Self::try_open`] / [`super::open_encoder`].
    #[doc(hidden)]
    pub fn inert_for_tests(reason: impl Into<String>) -> Self {
        Self {
            target_bitrate_bps: DEFAULT_TARGET_BITRATE_BPS,
            keyframe_pending: true,
            reason: reason.into(),
        }
    }

    /// Human-readable unavailability reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl H264Encoder for HardwareEncoderStub {
    type Error = EncodeError;

    fn encode(
        &mut self,
        _frame: &VideoFrame,
        _force_keyframe: bool,
    ) -> Result<EncodedAccessUnit, Self::Error> {
        Err(EncodeError::HardwareUnavailable(self.reason.clone()))
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
        EncoderBackendKind::Hardware
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{PixelFormat, VideoFrame};
    use std::time::Duration;

    #[test]
    fn try_open_always_unavailable() {
        let err = HardwareEncoderStub::try_open(&EncoderConfig::default()).unwrap_err();
        assert!(matches!(err, EncodeError::HardwareUnavailable(_)));
        assert!(err.to_string().contains("NVENC") || err.to_string().contains("not linked"));
    }

    #[test]
    fn inert_stub_errors_on_encode_but_accepts_feedback() {
        let mut hw = HardwareEncoderStub::inert_for_tests("test");
        assert_eq!(hw.backend_kind(), EncoderBackendKind::Hardware);
        hw.set_target_bitrate_bps(800_000);
        assert_eq!(hw.target_bitrate_bps(), 800_000);
        hw.request_keyframe();
        let frame = VideoFrame::packed(Duration::ZERO, 2, 2, PixelFormat::Bgra8, vec![0; 16]);
        assert!(matches!(
            hw.encode(&frame, true),
            Err(EncodeError::HardwareUnavailable(_))
        ));
    }
}
