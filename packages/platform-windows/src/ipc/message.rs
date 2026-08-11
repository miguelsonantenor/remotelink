//! Control-plane IPC message catalog (service ↔ session agent).
//!
//! **No media payload variants** exist by design (KD5): video NALUs, Opus/PCM,
//! and RTCP feedback stay inside the session agent with PeerTransport.
//!
//! Wire format wraps each message in [`ControlEnvelope`] (`v` + nested
//! `message`) so peers can fail closed on protocol skew.

use serde::{Deserialize, Serialize};

/// Protocol version for control IPC framing payloads (on the wire via envelope).
pub const CONTROL_IPC_VERSION: u32 = 1;

/// Max UTF-8 bytes for session ids.
pub const MAX_SESSION_ID_LEN: usize = 128;
/// Max UTF-8 bytes for short strings (labels, kinds, codes, ICE path, etc.).
pub const MAX_SHORT_STRING_LEN: usize = 512;
/// Max UTF-8 bytes for opaque signaling payloads (SDP / ICE JSON).
pub const MAX_SIGNAL_PAYLOAD_LEN: usize = 128 * 1024;
/// Max TURN URI entries per attach.
pub const MAX_TURN_URIS: usize = 16;
/// Max UTF-8 bytes per TURN URI.
pub const MAX_TURN_URI_LEN: usize = 512;
/// Max free-form feature flag strings.
pub const MAX_FEATURE_EXTRA: usize = 32;
/// Max UTF-8 bytes for boot secret.
pub const MAX_BOOT_SECRET_LEN: usize = 256;

/// Wire envelope: version + control message body.
///
/// JSON shape: `{"v":1,"message":{"method":"…","params":{…}}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlEnvelope {
    /// Protocol version ([`CONTROL_IPC_VERSION`]).
    pub v: u32,
    /// Control method payload.
    pub message: ControlMessage,
}

impl ControlEnvelope {
    /// Wrap a message at the current protocol version.
    pub fn new(message: ControlMessage) -> Self {
        Self {
            v: CONTROL_IPC_VERSION,
            message,
        }
    }
}

/// Top-level control IPC methods.
///
/// Tagged enum — each variant is a control method. Media byte methods such as
/// `PushVideoNalu` / `PushAudioFrame` are intentionally **absent**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Bind agent to a session with feature flags and TURN URIs (S→A).
    AttachSession(AttachSession),
    /// Unbind agent from the current session (S→A).
    DetachSession(DetachSession),
    /// Opaque signaling payloads: SDP, ICE, auth, session_end, etc. (S↔A).
    SignalForward(SignalForward),
    /// Session policy: input enablement, unattended, bitrate, encode flags (S→A).
    SetPolicy(SetPolicy),
    /// Start capture + encode + PeerTransport in the agent (S→A).
    StartMedia(StartMedia),
    /// Stop capture + encode + PeerTransport in the agent (S→A).
    StopMedia(StopMedia),
    /// Request stats snapshot from agent (S→A); agent may also push.
    QueryStats(QueryStats),
    /// Stats push from agent for tray/metrics (A→S).
    StatsPush(StatsPush),
    /// Force session border / top-bar connection chrome (S→A).
    ShowSessionChrome(ShowSessionChrome),
    /// Teardown PeerConnection + capture for a session (S→A).
    ShutdownSession(ShutdownSession),
    /// Immediate disconnect; disable input; optional unattended disable (S→A).
    KillSwitch(KillSwitch),
    /// User accepted/denied incoming session UI (A→S).
    LocalConfirmResult(LocalConfirmResult),
    /// Generic success ack.
    Ack(Ack),
    /// Generic error response.
    Error(ControlError),
}

impl ControlMessage {
    /// Stable method name string (matches serde `snake_case` tag).
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::AttachSession(_) => "attach_session",
            Self::DetachSession(_) => "detach_session",
            Self::SignalForward(_) => "signal_forward",
            Self::SetPolicy(_) => "set_policy",
            Self::StartMedia(_) => "start_media",
            Self::StopMedia(_) => "stop_media",
            Self::QueryStats(_) => "query_stats",
            Self::StatsPush(_) => "stats_push",
            Self::ShowSessionChrome(_) => "show_session_chrome",
            Self::ShutdownSession(_) => "shutdown_session",
            Self::KillSwitch(_) => "kill_switch",
            Self::LocalConfirmResult(_) => "local_confirm_result",
            Self::Ack(_) => "ack",
            Self::Error(_) => "error",
        }
    }

    /// All known control method names (for inventory / negative media checks).
    pub fn all_method_names() -> &'static [&'static str] {
        &[
            "attach_session",
            "detach_session",
            "signal_forward",
            "set_policy",
            "start_media",
            "stop_media",
            "query_stats",
            "stats_push",
            "show_session_chrome",
            "shutdown_session",
            "kill_switch",
            "local_confirm_result",
            "ack",
            "error",
        ]
    }

    /// Enforce per-field size caps (control-only KD5; no multi-MiB smuggling).
    pub fn validate_field_limits(&self) -> Result<(), FieldLimitError> {
        match self {
            Self::AttachSession(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(v) = &m.viewer_label {
                    check_str("viewer_label", v, MAX_SHORT_STRING_LEN)?;
                }
                if let Some(s) = &m.boot_secret {
                    check_str("boot_secret", s, MAX_BOOT_SECRET_LEN)?;
                }
                if m.turn_uris.len() > MAX_TURN_URIS {
                    return Err(FieldLimitError {
                        field: "turn_uris",
                        got: m.turn_uris.len(),
                        max: MAX_TURN_URIS,
                    });
                }
                for uri in &m.turn_uris {
                    check_str("turn_uris[]", uri, MAX_TURN_URI_LEN)?;
                }
                if m.feature_flags.extra.len() > MAX_FEATURE_EXTRA {
                    return Err(FieldLimitError {
                        field: "feature_flags.extra",
                        got: m.feature_flags.extra.len(),
                        max: MAX_FEATURE_EXTRA,
                    });
                }
                for s in &m.feature_flags.extra {
                    check_str("feature_flags.extra[]", s, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::DetachSession(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(r) = &m.reason {
                    check_str("reason", r, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::SignalForward(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                check_str("kind", &m.kind, MAX_SHORT_STRING_LEN)?;
                check_str("payload", &m.payload, MAX_SIGNAL_PAYLOAD_LEN)?;
            }
            Self::SetPolicy(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
            }
            Self::StartMedia(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(d) = &m.display_id {
                    check_str("display_id", d, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::StopMedia(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
            }
            Self::QueryStats(m) => {
                if let Some(s) = &m.session_id {
                    check_str("session_id", s, MAX_SESSION_ID_LEN)?;
                }
            }
            Self::StatsPush(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(p) = &m.ice_path {
                    check_str("ice_path", p, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::ShowSessionChrome(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(l) = &m.label {
                    check_str("label", l, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::ShutdownSession(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(r) = &m.reason {
                    check_str("reason", r, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::KillSwitch(m) => {
                if let Some(s) = &m.session_id {
                    check_str("session_id", s, MAX_SESSION_ID_LEN)?;
                }
            }
            Self::LocalConfirmResult(m) => {
                check_str("session_id", &m.session_id, MAX_SESSION_ID_LEN)?;
                if let Some(r) = &m.reason {
                    check_str("reason", r, MAX_SHORT_STRING_LEN)?;
                }
            }
            Self::Ack(m) => {
                if let Some(s) = &m.for_method {
                    check_str("for_method", s, MAX_SHORT_STRING_LEN)?;
                }
                if let Some(s) = &m.session_id {
                    check_str("session_id", s, MAX_SESSION_ID_LEN)?;
                }
            }
            Self::Error(m) => {
                check_str("code", &m.code, MAX_SHORT_STRING_LEN)?;
                check_str("message", &m.message, MAX_SHORT_STRING_LEN)?;
                if let Some(s) = &m.session_id {
                    check_str("session_id", s, MAX_SESSION_ID_LEN)?;
                }
            }
        }
        Ok(())
    }
}

fn check_str(field: &'static str, value: &str, max: usize) -> Result<(), FieldLimitError> {
    if value.len() > max {
        Err(FieldLimitError {
            field,
            got: value.len(),
            max,
        })
    } else {
        Ok(())
    }
}

/// Per-field size limit violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldLimitError {
    /// Field path.
    pub field: &'static str,
    /// Observed size (bytes or element count).
    pub got: usize,
    /// Maximum allowed.
    pub max: usize,
}

impl std::fmt::Display for FieldLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "field `{}` too large: {} (max {})",
            self.field, self.got, self.max
        )
    }
}

impl std::error::Error for FieldLimitError {}

/// Stable agent/service error codes for control handling.
pub mod error_codes {
    /// Request targeted a session that is not attached.
    pub const NOT_ATTACHED: &str = "not_attached";
    /// Request session_id does not match the attached session.
    pub const SESSION_MISMATCH: &str = "session_mismatch";
    /// Kill-switch is latched; media/input commands are refused.
    pub const KILLED: &str = "killed";
    /// Host already has an active session (single-controller / G9).
    pub const BUSY: &str = "busy";
    /// Mandatory session chrome cannot be hidden while a session is live (G9).
    pub const CHROME_MANDATORY: &str = "chrome_mandatory";
    /// Unattended Mode B is latched off by local kill-switch until re-enabled.
    pub const UNATTENDED_DISABLED: &str = "unattended_disabled";
    /// Message not expected in this role/direction.
    pub const UNEXPECTED: &str = "unexpected";
}

/// Media-related method names that must **never** appear on the control IPC.
pub const FORBIDDEN_MEDIA_METHODS: &[&str] = &[
    "push_video_nalu",
    "push_audio_frame",
    "push_pcm",
    "on_rtcp_feedback",
    "inject_rtcp",
    "raw_frame",
    "media_bytes",
];

/// Bind the session agent to a remote session (service → agent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachSession {
    /// Session identifier from the signaling broker.
    pub session_id: String,
    /// Viewer display / public id for chrome (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer_label: Option<String>,
    /// Feature flags from service / server config.
    #[serde(default)]
    pub feature_flags: FeatureFlags,
    /// TURN URIs (session-scoped credentials already applied by service).
    #[serde(default)]
    pub turn_uris: Vec<String>,
    /// Shared secret for this agent spawn (pipe auth complement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_secret: Option<String>,
}

/// Detach agent from its session without full process exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachSession {
    /// Session to detach; must match attached session when set.
    pub session_id: String,
    /// Human-readable reason for logs/UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Opaque signaling payload forwarded between service WS and agent PeerTransport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalForward {
    /// Session this signaling belongs to.
    pub session_id: String,
    /// Signaling kind (e.g. `session_offer`, `ice_candidate`, `auth_challenge`).
    pub kind: String,
    /// Opaque JSON or base64 body; service does not interpret media.
    pub payload: String,
    /// Who originated this forward hop.
    pub from: SignalHop,
}

/// Which process last produced a [`SignalForward`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalHop {
    /// Came from service / server WS.
    Service,
    /// Came from agent / PeerTransport.
    Agent,
}

/// Session policy applied by the service after auth/bind decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPolicy {
    /// Target session.
    pub session_id: String,
    /// Allow input injection only after identity bind + auth.
    #[serde(default)]
    pub enable_input: bool,
    /// Unattended (Mode B) allowed for this host/session.
    #[serde(default)]
    pub unattended: bool,
    /// Encoder bitrate cap in bits per second (0 = default profile).
    #[serde(default)]
    pub max_bitrate_bps: u32,
    /// Force software encode path.
    #[serde(default)]
    pub disable_hw_encode: bool,
}

/// Instruct agent to start media plane (capture/encode/PeerTransport).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartMedia {
    /// Session whose media plane should start.
    pub session_id: String,
    /// Optional display id (v1: single selected display; reserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
}

/// Instruct agent to stop media plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopMedia {
    /// Session whose media plane should stop.
    pub session_id: String,
}

/// Request a stats snapshot from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryStats {
    /// Session to query; omit for agent-wide aggregate (future).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Stats payload pushed or returned by the agent (no media samples).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsPush {
    /// Session these stats refer to.
    pub session_id: String,
    /// Approximate RTT in milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Outbound video bitrate estimate (bps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate_bps: Option<u32>,
    /// Outbound audio bitrate estimate (bps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bitrate_bps: Option<u32>,
    /// ICE path summary (e.g. `host`, `srflx`, `relay`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_path: Option<String>,
    /// A/V skew estimate in milliseconds (agent-local).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av_skew_ms: Option<f64>,
    /// Frames per second (encode or capture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    /// Packet loss fraction 0.0–1.0 if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<f64>,
}

/// Show mandatory connection chrome in the interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowSessionChrome {
    /// Session for which chrome is shown.
    pub session_id: String,
    /// Whether chrome should be visible.
    pub visible: bool,
    /// Optional label (viewer id / name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Full teardown of agent session resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownSession {
    /// Session to shut down.
    pub session_id: String,
    /// Reason code for audit/logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Immediate kill-switch: disconnect, disable input, optional unattended off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSwitch {
    /// Optional session scope; `None` means all active sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Also disable unattended Mode B until re-enabled locally.
    #[serde(default)]
    pub disable_unattended: bool,
    /// Origin of the kill (hotkey, tray, policy).
    #[serde(default)]
    pub source: KillSwitchSource,
}

/// Who triggered the kill-switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillSwitchSource {
    /// Default / unspecified.
    #[default]
    Unspecified,
    /// Global hotkey registered by the service.
    Hotkey,
    /// Tray UI action.
    Tray,
    /// Policy or remote admin force-disconnect.
    Policy,
}

/// Result of local accept/deny UI in the agent (or tray-coordinated UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfirmResult {
    /// Session being confirmed.
    pub session_id: String,
    /// Whether the user accepted the session.
    pub accepted: bool,
    /// Optional deny reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Feature flags delivered at attach time.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FeatureFlags {
    /// Allow synthetic media path (dev/CI).
    #[serde(default)]
    pub synthetic_media: bool,
    /// Stats HUD / detailed metrics.
    #[serde(default)]
    pub stats_export: bool,
    /// Extra free-form flags from server config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

/// Positive acknowledgement for a prior request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// Optional correlation / request method name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_method: Option<String>,
    /// Optional session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Error returned over the control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    /// Stable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}
