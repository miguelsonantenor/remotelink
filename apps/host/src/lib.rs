//! RemoteLink host library: service control plane + session agent media plane.
//!
//! KD5: service owns enrollment/signaling/policy; agent owns PeerTransport and
//! capture/encode. Control IPC is length-prefixed JSON with **no media bytes**.
//!
//! G9: service-owned session indicator + local kill-switch.

#![deny(missing_docs)]

pub mod agent;
pub mod chrome;
pub mod platform_capture;
pub mod policy;
pub mod service;
pub mod session;

pub use agent::{local_confirm, run_agent_only_synthetic, AgentSession, AgentSessionState};
pub use chrome::{HostSessionUx, SessionChrome, SessionIndicator};
pub use platform_capture::{
    default_audio_kind, default_video_kind, open_audio_source, open_default_sources,
    open_video_source, AudioCaptureKind, HostAudioSource, HostVideoSource, PlatformCaptureError,
    VideoCaptureKind,
};
pub use policy::{HostAuthService, HostLocalConfig, DEFAULT_HOST_OTP_PEPPER};
pub use service::{
    build_session_start_sequence, run_colocate_synthetic, run_kill_switch_demo,
    service_kill_switch, signal_to_agent,
};
pub use session::{
    parse_ice_payload, parse_sdp_payload, signal_kind, InboundStats, InputProcessOutcome,
    PumpStats, SdpPayload, SessionError, SessionManager, INPUT_CHANNEL_LABEL,
};
