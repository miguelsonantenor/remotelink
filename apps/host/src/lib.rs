//! RemoteLink host library: service control plane + session agent media plane.
//!
//! KD5: service owns enrollment/signaling/policy; agent owns PeerTransport and
//! capture/encode. Control IPC is length-prefixed JSON with **no media bytes**.
//!
//! PR 14: [`policy`] owns Mode A OTP mint/consume windows and Mode B
//! `unattended_enabled` gating; tray stubs log OTP codes to the CLI.

#![deny(missing_docs)]

pub mod agent;
pub mod policy;
pub mod service;
pub mod session;

pub use agent::{local_confirm, run_agent_only_synthetic, AgentSession, AgentSessionState};
pub use policy::{
    log_confirm_prompt, log_otp_to_cli, ActiveOtpWindow, ConfirmDecision, HostAuthService,
    HostLocalConfig, PolicyError, DEFAULT_HOST_OTP_PEPPER, DEFAULT_OTP_TTL_SECS,
};
pub use service::{
    build_session_start_sequence, run_colocate_synthetic, service_kill_switch, signal_to_agent,
};
pub use session::{
    parse_ice_payload, parse_sdp_payload, signal_kind, InboundStats, PumpStats, SdpPayload,
    SessionError, SessionManager, INPUT_CHANNEL_LABEL,
};
