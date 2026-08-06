//! Payload size limits for signaling fields.
//!
//! These limits are enforced by [`crate::decode_message`] (and optionally
//! [`crate::validate_message_limits`]) so callers get a bounded parse path.
//! Transport layers should still cap WebSocket frame size independently.

use crate::error::ProtocolError;
use crate::message::SignalMessage;

/// Maximum allowed SDP string length in bytes (UTF-8).
pub const MAX_SDP_BYTES: usize = 64 * 1024;

/// Maximum allowed ICE candidate string length in bytes (UTF-8).
pub const MAX_ICE_CANDIDATE_BYTES: usize = 4 * 1024;

/// Maximum allowed fingerprint signature string length in bytes.
pub const MAX_FINGERPRINT_SIG_BYTES: usize = 8 * 1024;

/// Maximum allowed opaque auth/stats/prefilter payload serialized size in bytes.
pub const MAX_OPAQUE_PAYLOAD_BYTES: usize = 16 * 1024;

fn check_str(field: &'static str, value: &str, max: usize) -> Result<(), ProtocolError> {
    let len = value.len();
    if len > max {
        return Err(ProtocolError::too_large(field, len, max));
    }
    Ok(())
}

fn check_json_value(field: &'static str, value: &serde_json::Value) -> Result<(), ProtocolError> {
    // Measure compact serialization size of opaque objects.
    let encoded = serde_json::to_string(value).map_err(ProtocolError::from)?;
    check_str(field, &encoded, MAX_OPAQUE_PAYLOAD_BYTES)
}

/// Validate that large/stringy fields on a decoded message respect size limits.
pub fn validate_message_limits(msg: &SignalMessage) -> Result<(), ProtocolError> {
    match msg {
        SignalMessage::Hello { .. }
        | SignalMessage::HelloOk { .. }
        | SignalMessage::SessionAccept { .. }
        | SignalMessage::SessionReject { .. }
        | SignalMessage::MediaRestart { .. }
        | SignalMessage::Renegotiate { .. }
        | SignalMessage::SessionEnd { .. }
        | SignalMessage::Error { .. } => Ok(()),

        SignalMessage::SessionIntent { prefilter, .. } => check_json_value("prefilter", prefilter),
        SignalMessage::SessionIncoming { viewer_info, .. } => {
            check_json_value("viewer_info", viewer_info)
        }
        SignalMessage::AuthChallenge { payload, .. }
        | SignalMessage::AuthResponse { payload, .. }
        | SignalMessage::Stats { payload, .. } => check_json_value("payload", payload),

        SignalMessage::SessionOffer {
            sdp,
            fingerprint_sig,
            ..
        } => {
            check_str("sdp", sdp, MAX_SDP_BYTES)?;
            check_str(
                "fingerprint_sig",
                fingerprint_sig,
                MAX_FINGERPRINT_SIG_BYTES,
            )
        }
        SignalMessage::SessionAnswer { sdp, .. } => check_str("sdp", sdp, MAX_SDP_BYTES),
        SignalMessage::IceCandidate { candidate, .. } => {
            check_str("candidate", &candidate.candidate, MAX_ICE_CANDIDATE_BYTES)
        }
    }
}
