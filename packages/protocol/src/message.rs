//! WebSocket signaling messages (`/v1/ws`).
//!
//! Session-scoped variants carry a monotonic [`signal_seq`](SignalMessage) used
//! to ignore stale multi-node pub/sub deliveries (see DESIGN.md split-brain).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Connection role presented in `hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Viewer,
}

/// Session authorization mode on `session_intent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Otp,
    Unattended,
    Password,
}

/// Reasons a host/server may reject a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Busy,
    Auth,
    Policy,
}

/// Authentication material on `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAuth {
    pub device_token: String,
}

/// WebRTC-style ICE candidate object relayed over signaling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_m_line_index: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_fragment: Option<String>,
}

/// All WebSocket signaling message variants.
///
/// Wire format is a JSON object with a `type` discriminant (snake_case).
/// Every session-scoped variant includes `session_id` and monotonic
/// `signal_seq` (per-session; receivers ignore stale sequences).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    Hello {
        role: Role,
        protocol_version: u32,
        auth: HelloAuth,
    },
    HelloOk {
        server_time: String,
        feature_flags: Value,
    },
    SessionIntent {
        session_id: String,
        signal_seq: u64,
        host_public_id: String,
        mode: SessionMode,
        prefilter: Value,
    },
    SessionIncoming {
        session_id: String,
        signal_seq: u64,
        viewer_info: Value,
    },
    AuthChallenge {
        session_id: String,
        signal_seq: u64,
        payload: Value,
    },
    AuthResponse {
        session_id: String,
        signal_seq: u64,
        payload: Value,
    },
    SessionAccept {
        session_id: String,
        signal_seq: u64,
    },
    SessionReject {
        session_id: String,
        signal_seq: u64,
        reason: RejectReason,
    },
    SessionOffer {
        session_id: String,
        signal_seq: u64,
        sdp: String,
        fingerprint_sig: String,
    },
    SessionAnswer {
        session_id: String,
        signal_seq: u64,
        sdp: String,
    },
    IceCandidate {
        session_id: String,
        signal_seq: u64,
        candidate: IceCandidate,
    },
    MediaRestart {
        session_id: String,
        signal_seq: u64,
    },
    Renegotiate {
        session_id: String,
        signal_seq: u64,
    },
    SessionEnd {
        session_id: String,
        signal_seq: u64,
        reason: String,
    },
    Stats {
        session_id: String,
        signal_seq: u64,
        payload: Value,
    },
    Error {
        code: String,
        message: String,
    },
}

impl SignalMessage {
    /// Returns the session id when this is a session-scoped message.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::SessionIntent { session_id, .. }
            | Self::SessionIncoming { session_id, .. }
            | Self::AuthChallenge { session_id, .. }
            | Self::AuthResponse { session_id, .. }
            | Self::SessionAccept { session_id, .. }
            | Self::SessionReject { session_id, .. }
            | Self::SessionOffer { session_id, .. }
            | Self::SessionAnswer { session_id, .. }
            | Self::IceCandidate { session_id, .. }
            | Self::MediaRestart { session_id, .. }
            | Self::Renegotiate { session_id, .. }
            | Self::SessionEnd { session_id, .. }
            | Self::Stats { session_id, .. } => Some(session_id),
            Self::Hello { .. } | Self::HelloOk { .. } | Self::Error { .. } => None,
        }
    }

    /// Returns the monotonic signal sequence when present.
    pub fn signal_seq(&self) -> Option<u64> {
        match self {
            Self::SessionIntent { signal_seq, .. }
            | Self::SessionIncoming { signal_seq, .. }
            | Self::AuthChallenge { signal_seq, .. }
            | Self::AuthResponse { signal_seq, .. }
            | Self::SessionAccept { signal_seq, .. }
            | Self::SessionReject { signal_seq, .. }
            | Self::SessionOffer { signal_seq, .. }
            | Self::SessionAnswer { signal_seq, .. }
            | Self::IceCandidate { signal_seq, .. }
            | Self::MediaRestart { signal_seq, .. }
            | Self::Renegotiate { signal_seq, .. }
            | Self::SessionEnd { signal_seq, .. }
            | Self::Stats { signal_seq, .. } => Some(*signal_seq),
            Self::Hello { .. } | Self::HelloOk { .. } | Self::Error { .. } => None,
        }
    }
}
