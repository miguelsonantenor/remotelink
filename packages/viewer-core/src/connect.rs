//! Connect credentials and session-intent stubs (server call deferred).

use remotelink_protocol::{decode_message, encode_message, SessionMode, SignalMessage};
use serde_json::{json, Value};

use crate::error::{Result, ViewerError};

/// Auth material the connect UI collects (password, OTP, or unattended secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectSecret {
    /// Mode C password prefilter material.
    Password(String),
    /// Mode A short-lived OTP from the host tray/CLI.
    Otp(String),
    /// Mode B unattended host secret (viewer-side copy; never sent as plaintext
    /// proof — only length/hint in prefilter; MAC computed later).
    Unattended(String),
}

impl ConnectSecret {
    /// Raw secret string for prefilter / logging redaction.
    pub fn as_str(&self) -> &str {
        match self {
            ConnectSecret::Password(s) | ConnectSecret::Otp(s) | ConnectSecret::Unattended(s) => s,
        }
    }

    /// Session mode implied by this secret kind.
    pub fn mode(&self) -> SessionMode {
        match self {
            ConnectSecret::Password(_) => SessionMode::Password,
            ConnectSecret::Otp(_) => SessionMode::Otp,
            ConnectSecret::Unattended(_) => SessionMode::Unattended,
        }
    }
}

/// Fields from the viewer Connect UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    /// Host public device id (from host tray / enrollment).
    pub host_public_id: String,
    /// Password or OTP.
    pub secret: ConnectSecret,
    /// Optional viewer display label for session_incoming.
    pub viewer_label: Option<String>,
}

impl ConnectRequest {
    /// Build a password-mode connect request.
    pub fn password(host_public_id: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            host_public_id: host_public_id.into(),
            secret: ConnectSecret::Password(password.into()),
            viewer_label: None,
        }
    }

    /// Build an OTP-mode connect request.
    pub fn otp(host_public_id: impl Into<String>, otp: impl Into<String>) -> Self {
        Self {
            host_public_id: host_public_id.into(),
            secret: ConnectSecret::Otp(otp.into()),
            viewer_label: None,
        }
    }

    /// Build an unattended Mode B connect request.
    pub fn unattended(host_public_id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            host_public_id: host_public_id.into(),
            secret: ConnectSecret::Unattended(secret.into()),
            viewer_label: None,
        }
    }

    /// Attach a viewer display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.viewer_label = Some(label.into());
        self
    }

    /// Validate non-empty host id and secret.
    pub fn validate(&self) -> Result<()> {
        if self.host_public_id.trim().is_empty() {
            return Err(ViewerError::InvalidConnect(
                "host public id is required".into(),
            ));
        }
        if self.secret.as_str().trim().is_empty() {
            return Err(ViewerError::InvalidConnect(
                "password, OTP, or unattended secret is required".into(),
            ));
        }
        if let ConnectSecret::Otp(otp) = &self.secret {
            // Soft check: OTP is typically 6–8 digits; allow alphanumeric for stubs.
            if otp.len() < 4 || otp.len() > 16 {
                return Err(ViewerError::InvalidConnect(
                    "OTP length must be between 4 and 16".into(),
                ));
            }
        }
        if let ConnectSecret::Unattended(s) = &self.secret {
            if s.len() < 8 {
                return Err(ViewerError::InvalidConnect(
                    "unattended secret looks too short".into(),
                ));
            }
        }
        Ok(())
    }

    /// Session mode for `session_intent`.
    pub fn mode(&self) -> SessionMode {
        self.secret.mode()
    }

    /// Opaque prefilter payload for server-side checks (stub; not a host auth proof).
    ///
    /// Mode A includes plaintext OTP for server hash prefilter (host already
    /// published the hash). Mode B never sends the secret — only a length hint.
    pub fn prefilter(&self) -> Value {
        match &self.secret {
            ConnectSecret::Password(p) => json!({ "password_hint_len": p.len() }),
            ConnectSecret::Otp(o) => json!({ "otp": o }),
            ConnectSecret::Unattended(s) => json!({ "unattended_hint_len": s.len() }),
        }
    }

    /// Build a typed `session_intent` signaling message (protocol wire authority).
    ///
    /// Does not open a WebSocket; used by UI stubs and unit tests.
    pub fn session_intent_message(
        &self,
        session_id: impl Into<String>,
        signal_seq: u64,
    ) -> Result<SignalMessage> {
        self.validate()?;
        Ok(SignalMessage::SessionIntent {
            session_id: session_id.into(),
            signal_seq,
            host_public_id: self.host_public_id.clone(),
            mode: self.mode(),
            prefilter: self.prefilter(),
        })
    }

    /// Encode a `session_intent` via [`encode_message`] and return JSON [`Value`].
    ///
    /// Round-trips through [`decode_message`] so payload size limits apply.
    pub fn session_intent_stub(&self, session_id: &str, signal_seq: u64) -> Result<Value> {
        let msg = self.session_intent_message(session_id, signal_seq)?;
        let wire = encode_message(&msg)?;
        let checked = decode_message(&wire)?;
        debug_assert_eq!(checked, msg);
        serde_json::from_str(&wire).map_err(|e| ViewerError::Internal(e.to_string()))
    }
}

/// Outcome of a stubbed "call server" connect attempt (no real network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectStubResult {
    /// Allocated session id (stub).
    pub session_id: String,
    /// Whether the stub accepted credentials shape (always true if validated).
    pub accepted: bool,
}

/// Stub signaling connect: validates credentials and mints a session id.
///
/// Real WSS hello / session_intent lands in a later PR; this keeps the Connect
/// UI and core testable without a running server.
pub fn connect_stub(req: &ConnectRequest) -> Result<ConnectStubResult> {
    req.validate()?;
    // Deterministic-ish id for tests: hash not required; use host id prefix.
    let session_id = format!("sess-stub-{}", sanitize_id(&req.host_public_id));
    Ok(ConnectStubResult {
        session_id,
        accepted: true,
    })
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_empty_host() {
        let r = ConnectRequest::password("", "secret");
        assert!(r.validate().is_err());
    }

    #[test]
    fn validates_empty_secret() {
        let r = ConnectRequest::otp("host1", "  ");
        assert!(r.validate().is_err());
    }

    #[test]
    fn otp_length_bounds() {
        assert!(ConnectRequest::otp("h", "12").validate().is_err());
        assert!(ConnectRequest::otp("h", "123456").validate().is_ok());
    }

    #[test]
    fn session_intent_stub_shape() {
        let r = ConnectRequest::otp("host-pub", "654321");
        let v = r.session_intent_stub("sess-1", 1).unwrap();
        assert_eq!(v["type"], "session_intent");
        assert_eq!(v["host_public_id"], "host-pub");
        assert_eq!(v["mode"], "otp");
        assert_eq!(v["prefilter"]["otp"], "654321");
    }

    #[test]
    fn unattended_session_intent_mode() {
        let r = ConnectRequest::unattended("host-pub", "host-local-secret!!");
        assert_eq!(r.mode(), SessionMode::Unattended);
        let v = r.session_intent_stub("sess-u", 1).unwrap();
        assert_eq!(v["mode"], "unattended");
        assert_eq!(v["prefilter"]["unattended_hint_len"], 19);
        // Secret must not appear in prefilter.
        let s = v["prefilter"].to_string();
        assert!(!s.contains("host-local-secret"));
    }

    #[test]
    fn session_intent_roundtrips_protocol_encode_decode() {
        let r = ConnectRequest::password("host-pub", "s3cret");
        let msg = r.session_intent_message("sess-1", 7).unwrap();
        let wire = encode_message(&msg).unwrap();
        let decoded = decode_message(&wire).unwrap();
        assert_eq!(decoded, msg);
        match decoded {
            SignalMessage::SessionIntent {
                session_id,
                signal_seq,
                host_public_id,
                mode,
                prefilter,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(signal_seq, 7);
                assert_eq!(host_public_id, "host-pub");
                assert_eq!(mode, SessionMode::Password);
                assert_eq!(prefilter["password_hint_len"], 6);
            }
            other => panic!("expected SessionIntent, got {other:?}"),
        }
        // Value stub matches the same wire document.
        let v = r.session_intent_stub("sess-1", 7).unwrap();
        let from_value: SignalMessage = serde_json::from_value(v).unwrap();
        assert_eq!(from_value, msg);
    }

    #[test]
    fn connect_stub_accepts_valid() {
        let r = ConnectRequest::password("abc", "pw").with_label("desk");
        let out = connect_stub(&r).unwrap();
        assert!(out.accepted);
        assert!(out.session_id.contains("abc"));
    }
}
