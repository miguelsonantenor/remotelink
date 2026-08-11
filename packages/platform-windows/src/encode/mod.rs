//! H.264 encode for the session agent (KD5 agent-media).
//!
//! Raw capture frames (BGRA/RGB from DXGI or mock) stay **in-process** and are
//! encoded to Annex-B NAL access units for [`remotelink_net::PeerTransport::send_video_nalu`].
//! Media bytes never cross control IPC.
//!
//! # Backends
//!
//! | Backend | When selected | CI-safe? |
//! |---------|---------------|----------|
//! | [`MockSoftwareEncoder`] | `disable_hw_encode` / `force_software`, or HW open fails | Yes |
//! | [`HardwareEncoderStub`] | Preferred when HW encode allowed | Open fails without GPU driver hooks |
//!
//! Hardware (NVENC / Quick Sync / AMF) is **documented as a stub** in this PR:
//! [`HardwareEncoderStub::try_open`] always returns [`EncodeError::HardwareUnavailable`]
//! so CI and windows-gnu builds never depend on a real GPU or proprietary SDK.
//! A later PR can replace the stub body without changing the trait surface.
//!
//! # Feedback
//!
//! PLI / FIR / GCC target-bitrate are applied in-process via
//! [`H264Encoder::request_keyframe`] and [`H264Encoder::set_target_bitrate_bps`]
//! (host `SessionManager` maps `ReceiverFeedback` → these calls).

mod h264;
mod hardware;
mod software;

pub use h264::{
    open_encoder, AnyH264Encoder, EncodeError, EncodedAccessUnit, EncoderBackendKind,
    EncoderConfig, H264Encoder, NaluFormat, DEFAULT_TARGET_BITRATE_BPS,
};
pub use hardware::HardwareEncoderStub;
pub use software::MockSoftwareEncoder;
