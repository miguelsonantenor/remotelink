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
//! | [`MockSoftwareEncoder`] | `disable_hw_encode` / `force_software`, or HW/MF open fails | Yes |
//! | Media Foundation H.264 MFT | Preferred on Windows when codec pack is present | May use GPU |
//! | [`HardwareEncoderStub`] | Direct NVENC/QSV/AMF (not linked yet) | Always unavailable |
//!
//! Production path tries **Media Foundation** (`CLSID_CMSH264EncoderMFT`) first,
//! then falls back to the mock software encoder. Direct vendor SDKs remain a
//! future replacement for the hardware stub.
//!
//! # Feedback
//!
//! PLI / FIR / GCC target-bitrate are applied in-process via
//! [`H264Encoder::request_keyframe`] and [`H264Encoder::set_target_bitrate_bps`]
//! (host `SessionManager` maps `ReceiverFeedback` → these calls).

mod h264;
mod hardware;
#[cfg(windows)]
mod mf;
mod software;

pub use h264::{
    open_encoder, AnyH264Encoder, EncodeError, EncodedAccessUnit, EncoderBackendKind,
    EncoderConfig, H264Encoder, NaluFormat, DEFAULT_TARGET_BITRATE_BPS,
};
pub use hardware::HardwareEncoderStub;
#[cfg(windows)]
pub use mf::MediaFoundationEncoder;
pub use software::MockSoftwareEncoder;
