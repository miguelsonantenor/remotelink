//! WebSocket signaling messages (`/v1/ws`).

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
        host_public_id: String,
        mode: SessionMode,
        prefilter: Value,
    },
    SessionIncoming {
        session_id: String,
        viewer_info: Value,
    },
    AuthChallenge {
        session_id: String,
        payload: Value,
    },
    AuthResponse {
        session_id: String,
        payload: Value,
    },
    SessionAccept {
        session_id: String,
    },
    SessionReject {
        session_id: String,
        reason: RejectReason,
    },
    SessionOffer {
        session_id: String,
        sdp: String,
        fingerprint_sig: String,
    },
    SessionAnswer {
        session_id: String,
        sdp: String,
    },
    IceCandidate {
        session_id: String,
        candidate: IceCandidate,
    },
    MediaRestart {
        session_id: String,
    },
    Renegotiate {
        session_id: String,
    },
    SessionEnd {
        session_id: String,
        reason: String,
    },
    Stats {
        session_id: String,
        payload: Value,
    },
    Error {
        code: String,
        message: String,
    },
}
