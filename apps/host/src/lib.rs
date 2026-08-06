//! RemoteLink host library: service control plane + session agent media plane.
//!
//! KD5: service owns enrollment/signaling/policy; agent owns PeerTransport and
//! capture/encode. Control IPC is length-prefixed JSON with **no media bytes**.

#![deny(missing_docs)]

pub mod agent;
pub mod service;
pub mod session;

pub use agent::{local_confirm, run_agent_only_synthetic, AgentSession, AgentSessionState};
pub use service::{
    build_session_start_sequence, run_colocate_synthetic, service_kill_switch, signal_to_agent,
};
pub use session::{
    parse_ice_payload, parse_sdp_payload, signal_kind, PumpStats, SdpPayload, SessionError,
    SessionManager,
};
