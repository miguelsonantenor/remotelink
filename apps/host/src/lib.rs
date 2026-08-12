//! RemoteLink host library: service control plane + session agent media plane.
//!
//! KD5: service owns enrollment/signaling/policy; agent owns PeerTransport and
//! capture/encode. Control IPC is length-prefixed JSON with **no media bytes**.
//!
//! G9: service-owned session indicator + local kill-switch.

#![deny(missing_docs)]

pub mod agent;
pub mod chrome;
pub mod control_loop;
pub mod platform_capture;
pub mod policy;
pub mod service;
pub mod session;
pub mod tray;
pub mod ws_session;

pub use agent::{
    local_confirm, run_agent_only_synthetic, run_with_transport, AgentSession, AgentSessionState,
};
pub use control_loop::{
    boot_secret_ok, format_endpoint, generate_boot_secret, parse_control_endpoint,
    run_agent_control_server, run_ipc_colocate_demo, serve_agent_connection, ServiceAgentClient,
    DRAIN_COMPLETE,
};
// Control IPC transport (TCP localhost for CI/dev). Re-exported so callers do not
// need a direct platform-windows dependency for KD5 service↔agent wiring.
pub use chrome::{HostSessionUx, SessionChrome, SessionIndicator};
pub use platform_capture::{
    default_audio_kind, default_video_kind, open_audio_source, open_default_sources,
    open_video_source, AudioCaptureKind, HostAudioSource, HostVideoSource, PlatformCaptureError,
    VideoCaptureKind,
};
pub use policy::{HostAuthService, HostLocalConfig, DEFAULT_HOST_OTP_PEPPER};
pub use remotelink_platform_windows::{
    listen_control, ControlEndpoint, ControlListener, ControlStream,
};
pub use service::{
    build_session_start_sequence, run_colocate_synthetic, run_kill_switch_demo,
    service_kill_switch, signal_to_agent,
};
pub use session::{
    parse_ice_payload, parse_sdp_payload, signal_kind, InboundStats, InputProcessOutcome,
    PumpStats, SdpPayload, SessionError, SessionManager, INPUT_CHANNEL_LABEL,
};
pub use tray::{default_status_path, HostTray, TrayCommands, TrayState};
pub use ws_session::{
    run_ws_host, run_ws_host_blocking, run_ws_host_service, ExistingHostCreds, WsHostConfig,
    DEFAULT_LAB_SERVER,
};
