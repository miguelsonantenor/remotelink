//! Length-prefixed JSON framed codec for control IPC.
//!
//! Frame layout:
//! ```text
//! +----------------+---------------------------+
//! | u32 BE length  | UTF-8 JSON payload bytes  |
//! +----------------+---------------------------+
//! ```
//!
//! Control messages use a versioned [`ControlEnvelope`] JSON body
//! (`{"v":1,"message":{…}}`). Length covers only the JSON payload.
//! Cap: [`MAX_FRAME_PAYLOAD`].

use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use super::message::{ControlEnvelope, ControlMessage, FieldLimitError, CONTROL_IPC_VERSION};

/// Maximum accepted JSON payload size (256 KiB).
///
/// Control-only KD5: large enough for SDP/`SignalForward` under
/// [`super::message::MAX_SIGNAL_PAYLOAD_LEN`], small enough to resist
/// media-like smuggling in opaque fields.
pub const MAX_FRAME_PAYLOAD: u32 = 256 * 1024;

/// Codec and framing errors.
#[derive(Debug, Error)]
pub enum CodecError {
    /// Payload length exceeds [`MAX_FRAME_PAYLOAD`].
    #[error("frame payload too large: {0} bytes (max {MAX_FRAME_PAYLOAD})")]
    PayloadTooLarge(u32),
    /// Declared length does not match available bytes when decoding a buffer.
    #[error("incomplete frame: need {need} bytes, have {have}")]
    Incomplete {
        /// Bytes required for a full frame (header + payload).
        need: usize,
        /// Bytes currently available.
        have: usize,
    },
    /// Buffer shorter than the 4-byte length header.
    #[error("frame header truncated")]
    HeaderTruncated,
    /// Envelope `v` is not [`CONTROL_IPC_VERSION`].
    #[error("unsupported control IPC version {0} (want {CONTROL_IPC_VERSION})")]
    UnsupportedVersion(u32),
    /// Per-field size limit exceeded.
    #[error(transparent)]
    FieldLimit(#[from] FieldLimitError),
    /// JSON serialization failure.
    #[error("json encode: {0}")]
    JsonEncode(#[source] serde_json::Error),
    /// JSON deserialization failure.
    #[error("json decode: {0}")]
    JsonDecode(#[source] serde_json::Error),
    /// Underlying I/O error while reading/writing a stream.
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Encode `value` as a length-prefixed JSON frame into a new buffer.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let payload = serde_json::to_vec(value).map_err(CodecError::JsonEncode)?;
    let len = u32::try_from(payload.len()).map_err(|_| CodecError::PayloadTooLarge(u32::MAX))?;
    if len > MAX_FRAME_PAYLOAD {
        return Err(CodecError::PayloadTooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode one length-prefixed JSON frame from `buf`.
///
/// Returns `(value, bytes_consumed)` so callers can advance a read buffer.
pub fn decode_frame<T: DeserializeOwned>(buf: &[u8]) -> Result<(T, usize), CodecError> {
    if buf.is_empty() {
        return Err(CodecError::HeaderTruncated);
    }
    if buf.len() < 4 {
        return Err(CodecError::HeaderTruncated);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_PAYLOAD {
        return Err(CodecError::PayloadTooLarge(len));
    }
    let payload_len = len as usize;
    let total = 4 + payload_len;
    if buf.len() < total {
        return Err(CodecError::Incomplete {
            need: total,
            have: buf.len(),
        });
    }
    let value: T = serde_json::from_slice(&buf[4..total]).map_err(CodecError::JsonDecode)?;
    Ok((value, total))
}

/// Encode a versioned [`ControlMessage`] frame (validates field limits).
pub fn encode_control(msg: &ControlMessage) -> Result<Vec<u8>, CodecError> {
    msg.validate_field_limits()?;
    encode_frame(&ControlEnvelope::new(msg.clone()))
}

/// Decode a versioned [`ControlMessage`] frame from a complete buffer slice.
pub fn decode_control(buf: &[u8]) -> Result<(ControlMessage, usize), CodecError> {
    let (env, consumed): (ControlEnvelope, usize) = decode_frame(buf)?;
    if env.v != CONTROL_IPC_VERSION {
        return Err(CodecError::UnsupportedVersion(env.v));
    }
    env.message.validate_field_limits()?;
    Ok((env.message, consumed))
}

/// Write one framed value to `w` (blocking). Prefer [`write_control`] for IPC.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<(), CodecError> {
    let frame = encode_frame(value)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Write one versioned control message to `w` (blocking).
pub fn write_control<W: Write>(w: &mut W, msg: &ControlMessage) -> Result<(), CodecError> {
    let frame = encode_control(msg)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Read exactly one framed value from `r` (blocking). Prefer [`read_control`].
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, CodecError> {
    let mut header = [0u8; 4];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(CodecError::HeaderTruncated);
        }
        Err(e) => return Err(CodecError::Io(e)),
    }
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_PAYLOAD {
        return Err(CodecError::PayloadTooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    serde_json::from_slice(&payload).map_err(CodecError::JsonDecode)
}

/// Read exactly one versioned control message from `r` (blocking).
pub fn read_control<R: Read>(r: &mut R) -> Result<ControlMessage, CodecError> {
    let env: ControlEnvelope = read_frame(r)?;
    if env.v != CONTROL_IPC_VERSION {
        return Err(CodecError::UnsupportedVersion(env.v));
    }
    env.message.validate_field_limits()?;
    Ok(env.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::message::*;
    use std::io::Cursor;

    fn sample_messages() -> Vec<ControlMessage> {
        vec![
            ControlMessage::AttachSession(AttachSession {
                session_id: "sess-1".into(),
                viewer_label: Some("viewer-a".into()),
                feature_flags: FeatureFlags {
                    synthetic_media: true,
                    stats_export: true,
                    extra: vec!["flag_x".into()],
                },
                turn_uris: vec!["turn:example:3478".into()],
                boot_secret: Some("secret".into()),
            }),
            ControlMessage::DetachSession(DetachSession {
                session_id: "sess-1".into(),
                reason: Some("idle".into()),
            }),
            ControlMessage::SignalForward(SignalForward {
                session_id: "sess-1".into(),
                kind: "session_offer".into(),
                payload: r#"{"sdp":"v=0..."}"#.into(),
                from: SignalHop::Service,
            }),
            ControlMessage::SetPolicy(SetPolicy {
                session_id: "sess-1".into(),
                enable_input: false,
                unattended: true,
                max_bitrate_bps: 8_000_000,
                disable_hw_encode: false,
            }),
            ControlMessage::StartMedia(StartMedia {
                session_id: "sess-1".into(),
                display_id: Some("display0".into()),
            }),
            ControlMessage::StopMedia(StopMedia {
                session_id: "sess-1".into(),
            }),
            ControlMessage::QueryStats(QueryStats {
                session_id: Some("sess-1".into()),
            }),
            ControlMessage::StatsPush(StatsPush {
                session_id: "sess-1".into(),
                rtt_ms: Some(12.5),
                video_bitrate_bps: Some(4_000_000),
                audio_bitrate_bps: Some(96_000),
                ice_path: Some("srflx".into()),
                av_skew_ms: Some(8.0),
                fps: Some(60.0),
                loss: Some(0.01),
            }),
            ControlMessage::ShowSessionChrome(ShowSessionChrome {
                session_id: "sess-1".into(),
                visible: true,
                label: Some("Alice".into()),
            }),
            ControlMessage::ShutdownSession(ShutdownSession {
                session_id: "sess-1".into(),
                reason: Some("user_hangup".into()),
            }),
            ControlMessage::KillSwitch(KillSwitch {
                session_id: None,
                disable_unattended: true,
                source: KillSwitchSource::Hotkey,
            }),
            ControlMessage::LocalConfirmResult(LocalConfirmResult {
                session_id: "sess-1".into(),
                accepted: true,
                reason: None,
            }),
            ControlMessage::Ack(Ack {
                for_method: Some("attach_session".into()),
                session_id: Some("sess-1".into()),
            }),
            ControlMessage::Error(ControlError {
                code: "busy".into(),
                message: "session already active".into(),
                session_id: Some("sess-1".into()),
            }),
        ]
    }

    #[test]
    fn roundtrip_every_control_message() {
        for msg in sample_messages() {
            let frame = encode_control(&msg).expect("encode");
            let (decoded, consumed) = decode_control(&frame).expect("decode");
            assert_eq!(consumed, frame.len());
            assert_eq!(decoded, msg, "roundtrip failed for {}", msg.method_name());
            // Envelope version is on the wire.
            let (env, _): (ControlEnvelope, _) = decode_frame(&frame).unwrap();
            assert_eq!(env.v, CONTROL_IPC_VERSION);
        }
    }

    #[test]
    fn stream_read_write_roundtrip() {
        let messages = sample_messages();
        let mut buf = Vec::new();
        for msg in &messages {
            write_control(&mut buf, msg).expect("write");
        }
        let mut cursor = Cursor::new(buf);
        for expected in &messages {
            let got = read_control(&mut cursor).expect("read");
            assert_eq!(&got, expected);
        }
    }

    #[test]
    fn rejects_oversized_declared_length() {
        let mut bad = (MAX_FRAME_PAYLOAD + 1).to_be_bytes().to_vec();
        bad.extend_from_slice(&[0u8; 8]);
        let err = decode_control(&bad).unwrap_err();
        assert!(matches!(err, CodecError::PayloadTooLarge(_)));
    }

    #[test]
    fn read_frame_rejects_oversized_declared_length() {
        let bad = (MAX_FRAME_PAYLOAD + 1).to_be_bytes().to_vec();
        // No need for a full payload; read_frame checks length before read_exact of body.
        let err = read_frame::<_, serde_json::Value>(&mut Cursor::new(bad)).unwrap_err();
        assert!(matches!(err, CodecError::PayloadTooLarge(_)));
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        // Craft a message whose JSON envelope exceeds MAX_FRAME_PAYLOAD.
        // SignalForward.payload is capped below the frame limit, so bypass
        // validate and call encode_frame directly with a huge blob.
        let huge = "x".repeat((MAX_FRAME_PAYLOAD as usize) + 1);
        let err = encode_frame(&huge).unwrap_err();
        assert!(matches!(err, CodecError::PayloadTooLarge(_)));
    }

    #[test]
    fn encode_control_rejects_oversized_signal_payload() {
        let msg = ControlMessage::SignalForward(SignalForward {
            session_id: "s".into(),
            kind: "offer".into(),
            payload: "p".repeat(MAX_SIGNAL_PAYLOAD_LEN + 1),
            from: SignalHop::Service,
        });
        let err = encode_control(&msg).unwrap_err();
        assert!(matches!(err, CodecError::FieldLimit(_)));
    }

    #[test]
    fn incomplete_frame_reports_need() {
        let msg = ControlMessage::StopMedia(StopMedia {
            session_id: "x".into(),
        });
        let frame = encode_control(&msg).unwrap();
        let err = decode_control(&frame[..frame.len() - 1]).unwrap_err();
        assert!(matches!(err, CodecError::Incomplete { .. }));
    }

    #[test]
    fn header_truncated_on_short_buffer() {
        let err = decode_control(&[0u8, 0, 1]).unwrap_err();
        assert!(matches!(err, CodecError::HeaderTruncated));
        let err = decode_control(&[]).unwrap_err();
        assert!(matches!(err, CodecError::HeaderTruncated));
    }

    #[test]
    fn rejects_unsupported_envelope_version() {
        let env = ControlEnvelope {
            v: CONTROL_IPC_VERSION + 99,
            message: ControlMessage::Ack(Ack {
                for_method: None,
                session_id: None,
            }),
        };
        let frame = encode_frame(&env).unwrap();
        let err = decode_control(&frame).unwrap_err();
        assert!(matches!(err, CodecError::UnsupportedVersion(_)));
    }

    #[test]
    fn no_forbidden_media_method_names_in_catalog() {
        let names = ControlMessage::all_method_names();
        for forbidden in FORBIDDEN_MEDIA_METHODS {
            assert!(
                !names.contains(forbidden),
                "control IPC must not include media method `{forbidden}`"
            );
        }
        let sample_names: Vec<_> = sample_messages().iter().map(|m| m.method_name()).collect();
        for name in names {
            assert!(
                sample_names.contains(name),
                "missing roundtrip sample for `{name}`"
            );
        }
    }

    #[test]
    fn serde_rejects_all_forbidden_media_method_tags() {
        for method in FORBIDDEN_MEDIA_METHODS {
            let json = format!(r#"{{"method":"{method}","params":{{}}}}"#);
            assert!(
                serde_json::from_str::<ControlMessage>(&json).is_err(),
                "expected serde reject for `{method}`"
            );
            let env_json = format!(r#"{{"v":1,"message":{json}}}"#);
            assert!(
                serde_json::from_str::<ControlEnvelope>(&env_json).is_err(),
                "expected envelope reject for `{method}`"
            );
        }
    }
}
