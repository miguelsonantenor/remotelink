//! RemoteLink signaling and input protocol schemas.
//!
//! Wire encoding is JSON via `serde_json`. See `DESIGN.md` WebSocket (`/v1/ws`)
//! and input path (v1 freeze) sections.
//!
//! # Size limits
//!
//! [`decode_message`] enforces [`MAX_SDP_BYTES`], [`MAX_ICE_CANDIDATE_BYTES`],
//! [`MAX_FINGERPRINT_SIG_BYTES`], and [`MAX_OPAQUE_PAYLOAD_BYTES`] on decoded
//! fields. Callers should still cap WebSocket frame size at the transport layer.
//!
//! # Input wire freeze
//!
//! [`InputEvent`] / [`MouseWheel`] etc. in this crate are the v1 field authority.
//! Wheel JSON is `{delta_x, delta_y, precise, x, y, display_id}` (not
//! `precise_delta`). Keys use Windows scan-set-1 scancodes via
//! [`lookup_scancode`] / [`NamedKey`] (not Unicode).

mod error;
mod input;
mod limits;
mod message;
mod scancode;

pub use error::ProtocolError;
pub use input::{
    modifiers, InputEvent, InputPayload, KeyEvent, MouseButton, MouseButtonKind, MouseMove,
    MouseWheel,
};
pub use limits::{
    validate_message_limits, MAX_FINGERPRINT_SIG_BYTES, MAX_ICE_CANDIDATE_BYTES,
    MAX_OPAQUE_PAYLOAD_BYTES, MAX_SDP_BYTES,
};
pub use message::{HelloAuth, IceCandidate, RejectReason, Role, SessionMode, SignalMessage};
pub use scancode::{lookup_scancode, named_key_from_char, scancode_of, NamedKey, ScanCode};

/// Current protocol version for `hello.protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Encode a value as a compact JSON string.
pub fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// Encode a value as pretty-printed JSON (tests / debugging).
pub fn encode_json_pretty<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

/// Decode a value from a JSON string slice (no size-limit checks).
pub fn decode_json<'a, T: serde::Deserialize<'a>>(s: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(s)
}

/// Encode a signaling message.
pub fn encode_message(msg: &SignalMessage) -> Result<String, ProtocolError> {
    Ok(encode_json(msg)?)
}

/// Decode a signaling message and enforce payload size limits.
pub fn decode_message(s: &str) -> Result<SignalMessage, ProtocolError> {
    let msg: SignalMessage = decode_json(s)?;
    validate_message_limits(&msg)?;
    Ok(msg)
}

/// Encode an input event.
pub fn encode_input(event: &InputEvent) -> Result<String, ProtocolError> {
    Ok(encode_json(event)?)
}

/// Decode an input event.
pub fn decode_input(s: &str) -> Result<InputEvent, ProtocolError> {
    Ok(decode_json(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_signal_roundtrip(msg: SignalMessage, golden: &str) {
        let encoded = encode_message(&msg).expect("encode");
        assert_eq!(encoded, golden, "golden encode mismatch for {msg:?}");
        let decoded = decode_message(&encoded).expect("decode");
        assert_eq!(decoded, msg, "roundtrip mismatch for {msg:?}");
        let from_golden = decode_message(golden).expect("decode golden");
        assert_eq!(from_golden, msg, "golden decode mismatch for {msg:?}");
    }

    fn assert_input_roundtrip(event: InputEvent, golden: &str) {
        let encoded = encode_input(&event).expect("encode");
        assert_eq!(encoded, golden, "golden encode mismatch for {event:?}");
        let decoded = decode_input(&encoded).expect("decode");
        assert_eq!(decoded, event, "roundtrip mismatch for {event:?}");
        let from_golden = decode_input(golden).expect("decode golden");
        assert_eq!(from_golden, event, "golden decode mismatch for {event:?}");
    }

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn limits_are_positive() {
        const {
            assert!(MAX_SDP_BYTES > 0);
            assert!(MAX_ICE_CANDIDATE_BYTES > 0);
            assert!(MAX_FINGERPRINT_SIG_BYTES > 0);
            assert!(MAX_OPAQUE_PAYLOAD_BYTES > 0);
        }
    }

    #[test]
    fn golden_hello() {
        assert_signal_roundtrip(
            SignalMessage::Hello {
                role: Role::Host,
                protocol_version: PROTOCOL_VERSION,
                auth: HelloAuth {
                    device_token: "tok-abc".into(),
                },
            },
            r#"{"type":"hello","role":"host","protocol_version":1,"auth":{"device_token":"tok-abc"}}"#,
        );
    }

    #[test]
    fn golden_hello_viewer() {
        assert_signal_roundtrip(
            SignalMessage::Hello {
                role: Role::Viewer,
                protocol_version: PROTOCOL_VERSION,
                auth: HelloAuth {
                    device_token: "viewer-tok".into(),
                },
            },
            r#"{"type":"hello","role":"viewer","protocol_version":1,"auth":{"device_token":"viewer-tok"}}"#,
        );
    }

    #[test]
    fn golden_hello_ok() {
        assert_signal_roundtrip(
            SignalMessage::HelloOk {
                server_time: "2026-01-01T00:00:00Z".into(),
                feature_flags: json!({"force_relay": false}),
            },
            r#"{"type":"hello_ok","server_time":"2026-01-01T00:00:00Z","feature_flags":{"force_relay":false}}"#,
        );
    }

    #[test]
    fn golden_session_intent() {
        assert_signal_roundtrip(
            SignalMessage::SessionIntent {
                session_id: "sess-1".into(),
                signal_seq: 1,
                host_public_id: "host-pub".into(),
                mode: SessionMode::Otp,
                prefilter: json!({"otp": "123456"}),
            },
            r#"{"type":"session_intent","session_id":"sess-1","signal_seq":1,"host_public_id":"host-pub","mode":"otp","prefilter":{"otp":"123456"}}"#,
        );
    }

    #[test]
    fn golden_session_intent_modes() {
        for (mode, wire) in [
            (SessionMode::Otp, "otp"),
            (SessionMode::Unattended, "unattended"),
            (SessionMode::Password, "password"),
        ] {
            let msg = SignalMessage::SessionIntent {
                session_id: "s".into(),
                signal_seq: 7,
                host_public_id: "h".into(),
                mode,
                prefilter: json!({}),
            };
            let encoded = encode_message(&msg).unwrap();
            assert!(
                encoded.contains(&format!(r#""mode":"{wire}""#)),
                "expected mode {wire} in {encoded}"
            );
            assert!(encoded.contains(r#""signal_seq":7"#));
            assert_eq!(decode_message(&encoded).unwrap(), msg);
        }
    }

    #[test]
    fn golden_session_incoming() {
        assert_signal_roundtrip(
            SignalMessage::SessionIncoming {
                session_id: "sess-1".into(),
                signal_seq: 2,
                viewer_info: json!({"display_name": "alice"}),
            },
            r#"{"type":"session_incoming","session_id":"sess-1","signal_seq":2,"viewer_info":{"display_name":"alice"}}"#,
        );
    }

    #[test]
    fn golden_auth_challenge() {
        assert_signal_roundtrip(
            SignalMessage::AuthChallenge {
                session_id: "sess-1".into(),
                signal_seq: 3,
                payload: json!({"nonce": "n1"}),
            },
            r#"{"type":"auth_challenge","session_id":"sess-1","signal_seq":3,"payload":{"nonce":"n1"}}"#,
        );
    }

    #[test]
    fn golden_auth_response() {
        assert_signal_roundtrip(
            SignalMessage::AuthResponse {
                session_id: "sess-1".into(),
                signal_seq: 4,
                payload: json!({"mac": "deadbeef"}),
            },
            r#"{"type":"auth_response","session_id":"sess-1","signal_seq":4,"payload":{"mac":"deadbeef"}}"#,
        );
    }

    #[test]
    fn golden_session_accept() {
        assert_signal_roundtrip(
            SignalMessage::SessionAccept {
                session_id: "sess-1".into(),
                signal_seq: 5,
            },
            r#"{"type":"session_accept","session_id":"sess-1","signal_seq":5}"#,
        );
    }

    #[test]
    fn golden_session_reject() {
        assert_signal_roundtrip(
            SignalMessage::SessionReject {
                session_id: "sess-1".into(),
                signal_seq: 6,
                reason: RejectReason::Busy,
            },
            r#"{"type":"session_reject","session_id":"sess-1","signal_seq":6,"reason":"busy"}"#,
        );
    }

    #[test]
    fn golden_session_reject_reasons() {
        for (reason, wire) in [
            (RejectReason::Busy, "busy"),
            (RejectReason::Auth, "auth"),
            (RejectReason::Policy, "policy"),
        ] {
            let msg = SignalMessage::SessionReject {
                session_id: "s".into(),
                signal_seq: 1,
                reason,
            };
            let encoded = encode_message(&msg).unwrap();
            assert!(encoded.contains(&format!(r#""reason":"{wire}""#)));
            assert_eq!(decode_message(&encoded).unwrap(), msg);
        }
    }

    #[test]
    fn golden_session_offer() {
        assert_signal_roundtrip(
            SignalMessage::SessionOffer {
                session_id: "sess-1".into(),
                signal_seq: 8,
                sdp: "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\n".into(),
                fingerprint_sig: "sig-bytes".into(),
            },
            r#"{"type":"session_offer","session_id":"sess-1","signal_seq":8,"sdp":"v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\n","fingerprint_sig":"sig-bytes"}"#,
        );
    }

    #[test]
    fn golden_session_answer() {
        assert_signal_roundtrip(
            SignalMessage::SessionAnswer {
                session_id: "sess-1".into(),
                signal_seq: 9,
                sdp: "v=0\r\n".into(),
            },
            r#"{"type":"session_answer","session_id":"sess-1","signal_seq":9,"sdp":"v=0\r\n"}"#,
        );
    }

    #[test]
    fn golden_ice_candidate() {
        assert_signal_roundtrip(
            SignalMessage::IceCandidate {
                session_id: "sess-1".into(),
                signal_seq: 10,
                candidate: IceCandidate {
                    candidate: "candidate:1 1 UDP 2122252543 192.0.2.1 54321 typ host".into(),
                    sdp_mid: Some("0".into()),
                    sdp_m_line_index: Some(0),
                    username_fragment: None,
                },
            },
            r#"{"type":"ice_candidate","session_id":"sess-1","signal_seq":10,"candidate":{"candidate":"candidate:1 1 UDP 2122252543 192.0.2.1 54321 typ host","sdp_mid":"0","sdp_m_line_index":0}}"#,
        );
    }

    #[test]
    fn golden_ice_candidate_full_optionals() {
        assert_signal_roundtrip(
            SignalMessage::IceCandidate {
                session_id: "sess-1".into(),
                signal_seq: 11,
                candidate: IceCandidate {
                    candidate: "candidate:2 1 UDP 1686052607 198.51.100.1 9 typ srflx".into(),
                    sdp_mid: Some("1".into()),
                    sdp_m_line_index: Some(1),
                    username_fragment: Some("ufrag1".into()),
                },
            },
            r#"{"type":"ice_candidate","session_id":"sess-1","signal_seq":11,"candidate":{"candidate":"candidate:2 1 UDP 1686052607 198.51.100.1 9 typ srflx","sdp_mid":"1","sdp_m_line_index":1,"username_fragment":"ufrag1"}}"#,
        );
    }

    #[test]
    fn golden_media_restart() {
        assert_signal_roundtrip(
            SignalMessage::MediaRestart {
                session_id: "sess-1".into(),
                signal_seq: 12,
            },
            r#"{"type":"media_restart","session_id":"sess-1","signal_seq":12}"#,
        );
    }

    #[test]
    fn golden_renegotiate() {
        assert_signal_roundtrip(
            SignalMessage::Renegotiate {
                session_id: "sess-1".into(),
                signal_seq: 13,
            },
            r#"{"type":"renegotiate","session_id":"sess-1","signal_seq":13}"#,
        );
    }

    #[test]
    fn golden_session_end() {
        assert_signal_roundtrip(
            SignalMessage::SessionEnd {
                session_id: "sess-1".into(),
                signal_seq: 14,
                reason: "user_hangup".into(),
            },
            r#"{"type":"session_end","session_id":"sess-1","signal_seq":14,"reason":"user_hangup"}"#,
        );
    }

    #[test]
    fn golden_stats() {
        // Free-form Value payloads: assert structural equality, not Map key order.
        let msg = SignalMessage::Stats {
            session_id: "sess-1".into(),
            signal_seq: 15,
            payload: json!({"rtt_ms": 12.5, "bitrate_kbps": 8000}),
        };
        let encoded = encode_message(&msg).expect("encode");
        assert!(encoded.contains(r#""type":"stats""#));
        assert!(encoded.contains(r#""session_id":"sess-1""#));
        assert!(encoded.contains(r#""signal_seq":15"#));
        let decoded = decode_message(&encoded).expect("decode");
        assert_eq!(decoded, msg);
        assert_eq!(decoded.signal_seq(), Some(15));
        assert_eq!(decoded.session_id(), Some("sess-1"));
    }

    #[test]
    fn golden_error() {
        assert_signal_roundtrip(
            SignalMessage::Error {
                code: "protocol_version".into(),
                message: "unsupported protocol_version".into(),
            },
            r#"{"type":"error","code":"protocol_version","message":"unsupported protocol_version"}"#,
        );
        let err = decode_message(
            r#"{"type":"error","code":"protocol_version","message":"unsupported protocol_version"}"#,
        )
        .unwrap();
        assert_eq!(err.signal_seq(), None);
        assert_eq!(err.session_id(), None);
    }

    #[test]
    fn golden_input_mouse_move() {
        assert_input_roundtrip(
            InputEvent {
                client_ts_us: 1_700_000_000_000_000,
                seq: 1,
                payload: InputPayload::MouseMove(MouseMove {
                    x: 0.5,
                    y: 0.25,
                    display_id: 0,
                }),
            },
            r#"{"client_ts_us":1700000000000000,"seq":1,"payload":{"kind":"mouse_move","x":0.5,"y":0.25,"display_id":0}}"#,
        );
    }

    #[test]
    fn golden_input_mouse_button() {
        assert_input_roundtrip(
            InputEvent {
                client_ts_us: 100,
                seq: 2,
                payload: InputPayload::MouseButton(MouseButton {
                    button: MouseButtonKind::Left,
                    pressed: true,
                    x: 0.1,
                    y: 0.2,
                    display_id: 0,
                }),
            },
            r#"{"client_ts_us":100,"seq":2,"payload":{"kind":"mouse_button","button":"left","pressed":true,"x":0.1,"y":0.2,"display_id":0}}"#,
        );
    }

    #[test]
    fn golden_input_mouse_buttons_all() {
        for (kind, wire) in [
            (MouseButtonKind::Left, "left"),
            (MouseButtonKind::Right, "right"),
            (MouseButtonKind::Middle, "middle"),
            (MouseButtonKind::X1, "x1"),
            (MouseButtonKind::X2, "x2"),
        ] {
            let event = InputEvent {
                client_ts_us: 1,
                seq: 1,
                payload: InputPayload::MouseButton(MouseButton {
                    button: kind,
                    pressed: false,
                    x: 0.0,
                    y: 0.0,
                    display_id: 0,
                }),
            };
            let encoded = encode_input(&event).unwrap();
            assert!(
                encoded.contains(&format!(r#""button":"{wire}""#)),
                "expected button wire {wire} in {encoded}"
            );
            assert_eq!(decode_input(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn golden_input_mouse_wheel() {
        assert_input_roundtrip(
            InputEvent {
                client_ts_us: 200,
                seq: 3,
                payload: InputPayload::MouseWheel(MouseWheel {
                    delta_x: 0.0,
                    delta_y: -1.0,
                    precise: false,
                    x: 0.5,
                    y: 0.5,
                    display_id: 0,
                }),
            },
            r#"{"client_ts_us":200,"seq":3,"payload":{"kind":"mouse_wheel","delta_x":0.0,"delta_y":-1.0,"precise":false,"x":0.5,"y":0.5,"display_id":0}}"#,
        );
    }

    #[test]
    fn golden_input_key_event() {
        assert_input_roundtrip(
            InputEvent {
                client_ts_us: 300,
                seq: 4,
                payload: InputPayload::Key(KeyEvent {
                    scancode: 0x1C,
                    extended: false,
                    pressed: true,
                    modifiers: modifiers::CTRL | modifiers::SHIFT,
                }),
            },
            r#"{"client_ts_us":300,"seq":4,"payload":{"kind":"key","scancode":28,"extended":false,"pressed":true,"modifiers":5}}"#,
        );
    }

    #[test]
    fn golden_input_key_extended() {
        assert_input_roundtrip(
            InputEvent {
                client_ts_us: 400,
                seq: 5,
                payload: InputPayload::Key(KeyEvent {
                    scancode: 0x48,
                    extended: true,
                    pressed: false,
                    modifiers: 0,
                }),
            },
            r#"{"client_ts_us":400,"seq":5,"payload":{"kind":"key","scancode":72,"extended":true,"pressed":false,"modifiers":0}}"#,
        );
    }

    #[test]
    fn reject_unknown_message_type() {
        let err = decode_message(r#"{"type":"not_a_real_type"}"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant"),
            "expected unknown variant wording, got: {msg}"
        );
    }

    #[test]
    fn reject_unknown_input_kind() {
        let err = decode_input(
            r#"{"client_ts_us":1,"seq":1,"payload":{"kind":"not_a_payload","x":0.0}}"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant"),
            "expected unknown variant wording, got: {msg}"
        );
    }

    #[test]
    fn reject_missing_required_fields() {
        let hello_no_auth =
            decode_message(r#"{"type":"hello","role":"host","protocol_version":1}"#);
        assert!(hello_no_auth.is_err(), "hello without auth must fail");

        let offer_no_sig = decode_message(
            r#"{"type":"session_offer","session_id":"s","signal_seq":1,"sdp":"v=0"}"#,
        );
        assert!(
            offer_no_sig.is_err(),
            "session_offer without fingerprint_sig must fail"
        );

        let intent_no_seq = decode_message(
            r#"{"type":"session_intent","session_id":"s","host_public_id":"h","mode":"otp","prefilter":{}}"#,
        );
        assert!(
            intent_no_seq.is_err(),
            "session_intent without signal_seq must fail"
        );
    }

    #[test]
    fn reject_oversized_sdp() {
        let huge = "x".repeat(MAX_SDP_BYTES + 1);
        let msg = SignalMessage::SessionAnswer {
            session_id: "s".into(),
            signal_seq: 1,
            sdp: huge,
        };
        // Bypass encode path: build JSON with oversized field.
        let json = format!(
            r#"{{"type":"session_answer","session_id":"s","signal_seq":1,"sdp":"{}"}}"#,
            "x".repeat(MAX_SDP_BYTES + 1)
        );
        let err = decode_message(&json).unwrap_err();
        match err {
            ProtocolError::PayloadTooLarge { field, len, max } => {
                assert_eq!(field, "sdp");
                assert_eq!(len, MAX_SDP_BYTES + 1);
                assert_eq!(max, MAX_SDP_BYTES);
            }
            other => panic!("expected PayloadTooLarge, got {other}"),
        }
        // validate_message_limits also rejects in-memory oversized values
        assert!(validate_message_limits(&msg).is_err());
    }

    #[test]
    fn reject_oversized_fingerprint_sig() {
        let json = format!(
            r#"{{"type":"session_offer","session_id":"s","signal_seq":1,"sdp":"v=0","fingerprint_sig":"{}"}}"#,
            "s".repeat(MAX_FINGERPRINT_SIG_BYTES + 1)
        );
        let err = decode_message(&json).unwrap_err();
        match err {
            ProtocolError::PayloadTooLarge { field, .. } => assert_eq!(field, "fingerprint_sig"),
            other => panic!("expected PayloadTooLarge, got {other}"),
        }
    }

    #[test]
    fn reject_oversized_ice_candidate() {
        let json = format!(
            r#"{{"type":"ice_candidate","session_id":"s","signal_seq":1,"candidate":{{"candidate":"{}"}}}}"#,
            "c".repeat(MAX_ICE_CANDIDATE_BYTES + 1)
        );
        let err = decode_message(&json).unwrap_err();
        match err {
            ProtocolError::PayloadTooLarge { field, .. } => assert_eq!(field, "candidate"),
            other => panic!("expected PayloadTooLarge, got {other}"),
        }
    }

    #[test]
    fn reject_malformed_json() {
        assert!(decode_message("{").is_err());
        assert!(decode_input("null").is_err());
    }
}
