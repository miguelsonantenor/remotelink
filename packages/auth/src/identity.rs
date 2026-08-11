//! Identity binding orchestration (KD17 / PR 13).
//!
//! # Trust model
//!
//! A compromised signaling server can substitute SDP `a=fingerprint` and MITM
//! DTLS. RemoteLink requires **both**:
//!
//! 1. **`session_authorized`** — Mode A (OTP) or Mode B (host-only challenge-
//!    response) verified on the host with host-held secrets.
//! 2. **`identity_bound`** — Host signs its DTLS fingerprint + `session_id` with
//!    the enrolled device key; viewer verifies. After DTLS connects, a
//!    DataChannel challenge binds `session_id || fp_host || fp_viewer` with
//!    session auth material.
//!
//! Host **MUST NOT** accept input until `identity_bound && session_authorized`
//! ([`IdentityBindState::input_allowed`]).
//!
//! # DTLS certificates (v1 mock vs later real)
//!
//! Real WebRTC DTLS certificates are **not** minted in this PR. The mock
//! PeerTransport exports synthetic fingerprints via
//! [`remotelink_net::DtlsFingerprint::sha256`] (parsed from mock SDP
//! `a=fingerprint:`). Production backends must export the **completed DTLS**
//! certificate fingerprint, not only the SDP line, before enabling input.
//!
//! # Wire: `fingerprint_sig`
//!
//! Host signs `encode_binding_message(session_id, fingerprint_sign_material)`
//! with the enrolled ed25519 device key. The signature is hex-encoded (128
//! lowercase hex chars) for `session_offer.fingerprint_sig`.
//!
//! # Wire: DataChannel identity challenge
//!
//! Label: [`IDENTITY_CHANNEL_LABEL`] (`"identity"`).
//!
//! - Host → viewer: `{"type":"dc_challenge","nonce":"<hex>"}`
//! - Viewer → host: `{"type":"dc_response","mac":"<hex>"}` where
//!   `mac = HMAC-SHA256(bind_key, domain || session_id || nonce || fp_host || fp_viewer)`.
//!
//! # Mode A bind key (viewer-computable)
//!
//! The host/server OTP **storage pepper** is used only for hashing OTP rows
//! ([`crate::otp`]). The post-DTLS DC bind key is derived from the **OTP digits
//! alone** with a public domain salt ([`OTP_BIND_KEY_SALT`]):
//! `HMAC-SHA256(key = OTP_BIND_KEY_SALT, message = otp_utf8)`.
//! Both host (after Mode A accept) and viewer (after user enters the code) can
//! compute the same key without shipping host-only pepper to the viewer.

use crate::challenge::{
    respond_to_challenge, verify_challenge_mac, AuthChallenge, ChallengeTranscript, HostSecret,
    CHALLENGE_MAC_LEN,
};
use crate::error::{AuthError, Result};
use crate::fingerprint::{sign_fingerprint_binding, verify_fingerprint_binding};
use crate::otp::OtpRecord;
use ed25519_dalek::{SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// DataChannel label for post-connect identity bind messages.
pub const IDENTITY_CHANNEL_LABEL: &str = "identity";

/// Domain separator for the DC identity bind MAC.
const DC_BIND_DOMAIN: &[u8] = b"remotelink-dc-identity-bind-v1";

/// Public domain salt for Mode A DC bind-key derivation.
///
/// This is **not** a secret and is **not** the host/server OTP storage pepper.
/// Both host and viewer derive
/// `HMAC-SHA256(key = OTP_BIND_KEY_SALT, message = otp_utf8)` from the
/// plaintext OTP digits the user typed / host accepted.
pub const OTP_BIND_KEY_SALT: &[u8] = b"remotelink-otp-bind-key-v1";

/// Nonce length for DC identity challenges (bytes).
pub const DC_CHALLENGE_NONCE_LEN: usize = 32;

/// MAC length for DC identity responses (SHA-256).
pub const DC_BIND_MAC_LEN: usize = 32;

/// Session auth material used to prove the DC identity bind (zeroized on drop).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SessionBindKey {
    bytes: Vec<u8>,
}

impl SessionBindKey {
    /// Wrap raw bind-key bytes (must be non-empty).
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(AuthError::Crypto(
                "session bind key must be non-empty".into(),
            ));
        }
        Ok(Self { bytes })
    }

    /// Derive a Mode A DC bind key from OTP digits alone (viewer-computable).
    ///
    /// Layout: `HMAC-SHA256(key = `[`OTP_BIND_KEY_SALT`]`, message = otp_utf8)`.
    ///
    /// The host/server OTP **storage pepper** is intentionally not an input:
    /// the viewer only knows the on-screen code. Pepper remains for
    /// [`crate::otp::hash_otp`] row storage only.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::InvalidOtpFormat`] when `otp` fails format checks.
    pub fn from_mode_a_otp(otp: &str) -> Result<Self> {
        crate::otp::validate_otp_format(otp)?;
        let mut mac =
            HmacSha256::new_from_slice(OTP_BIND_KEY_SALT).expect("HMAC accepts any key length");
        mac.update(otp.as_bytes());
        let result = mac.finalize().into_bytes();
        Ok(Self {
            bytes: result.to_vec(),
        })
    }

    /// Derive a bind key from a Mode B host-only secret (copy of secret bytes).
    pub fn from_mode_b_secret(secret: &HostSecret) -> Self {
        Self {
            bytes: secret.as_bytes().to_vec(),
        }
    }

    /// Borrow bind-key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for SessionBindKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionBindKey([redacted])")
    }
}

/// Combined authorization + identity-bind flags for one session.
///
/// Input is allowed only when **both** flags are true.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityBindState {
    /// Session identifier (opaque string / UUID).
    pub session_id: String,
    /// Mode A/B (or local policy) authorization completed.
    pub session_authorized: bool,
    /// Fingerprint sig verified + post-DTLS DC challenge completed.
    pub identity_bound: bool,
}

impl IdentityBindState {
    /// New unbound / unauthorized state for `session_id`.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            session_authorized: false,
            identity_bound: false,
        }
    }

    /// Host may accept remote input only when both gates pass.
    pub fn input_allowed(&self) -> bool {
        self.identity_bound && self.session_authorized
    }

    /// Error when input is not allowed (for host reject paths).
    pub fn input_gate_error(&self) -> AuthError {
        AuthError::InputNotAllowed {
            identity_bound: self.identity_bound,
            session_authorized: self.session_authorized,
        }
    }

    /// Mark session authorized after successful Mode A/B verification.
    pub fn mark_authorized(&mut self) {
        self.session_authorized = true;
    }

    /// Mark identity bound after successful fingerprint + DC challenge.
    pub fn mark_identity_bound(&mut self) {
        self.identity_bound = true;
    }

    /// Reset both flags (session end / kill).
    pub fn reset_flags(&mut self) {
        self.session_authorized = false;
        self.identity_bound = false;
    }
}

/// Sign host DTLS fingerprint for `session_offer.fingerprint_sig`.
///
/// `fingerprint_sign_material` must be the canonical form from
/// `DtlsFingerprint::as_sign_material()` (`sha-256 AA:BB:…`).
pub fn sign_session_fingerprint(
    signing_key: &SigningKey,
    session_id: &str,
    fingerprint_sign_material: &str,
) -> String {
    let sig = sign_fingerprint_binding(
        signing_key,
        session_id.as_bytes(),
        fingerprint_sign_material.as_bytes(),
    );
    bytes_to_hex(&sig)
}

/// Verify `fingerprint_sig` against the enrolled host public key.
pub fn verify_session_fingerprint(
    verifying_key: &VerifyingKey,
    session_id: &str,
    fingerprint_sign_material: &str,
    fingerprint_sig_hex: &str,
) -> Result<()> {
    let sig = hex_to_bytes(fingerprint_sig_hex).map_err(|_| AuthError::FingerprintSigInvalid)?;
    verify_fingerprint_binding(
        verifying_key,
        session_id.as_bytes(),
        fingerprint_sign_material.as_bytes(),
        &sig,
    )
}

/// Host-issued post-DTLS identity challenge (DataChannel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcIdentityChallenge {
    /// Random nonce (typically [`DC_CHALLENGE_NONCE_LEN`] bytes).
    pub nonce: Vec<u8>,
}

impl DcIdentityChallenge {
    /// Issue a fresh challenge nonce.
    pub fn issue() -> Self {
        let mut nonce = vec![0u8; DC_CHALLENGE_NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        Self { nonce }
    }

    /// Encode as JSON bytes for the identity DataChannel.
    pub fn encode(&self) -> Vec<u8> {
        format!(
            r#"{{"type":"dc_challenge","nonce":"{}"}}"#,
            bytes_to_hex(&self.nonce)
        )
        .into_bytes()
    }
}

/// Viewer response to a DC identity challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcIdentityResponse {
    /// HMAC-SHA256 tag ([`DC_BIND_MAC_LEN`] bytes).
    pub mac: [u8; DC_BIND_MAC_LEN],
}

impl DcIdentityResponse {
    /// Encode as JSON bytes for the identity DataChannel.
    pub fn encode(&self) -> Vec<u8> {
        format!(
            r#"{{"type":"dc_response","mac":"{}"}}"#,
            bytes_to_hex(&self.mac)
        )
        .into_bytes()
    }
}

/// Parsed identity DataChannel message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcIdentityMessage {
    /// Host → viewer challenge.
    Challenge(DcIdentityChallenge),
    /// Viewer → host response.
    Response(DcIdentityResponse),
}

impl DcIdentityMessage {
    /// Parse a JSON identity DataChannel payload.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(data)
            .map_err(|_| AuthError::IdentityBind("invalid utf-8".into()))?;
        parse_dc_json(s)
    }
}

fn parse_dc_json(s: &str) -> Result<DcIdentityMessage> {
    // Minimal hand-rolled parse to avoid pulling serde into auth for two fields.
    let trimmed = s.trim();
    if !trimmed.starts_with('{') {
        return Err(AuthError::IdentityBind("expected JSON object".into()));
    }
    let typ = json_string_field(trimmed, "type")
        .ok_or_else(|| AuthError::IdentityBind("missing type".into()))?;
    match typ.as_str() {
        "dc_challenge" => {
            let nonce_hex = json_string_field(trimmed, "nonce")
                .ok_or_else(|| AuthError::IdentityBind("missing nonce".into()))?;
            let nonce = hex_to_bytes(&nonce_hex)
                .map_err(|e| AuthError::IdentityBind(format!("nonce hex: {e}")))?;
            if nonce.is_empty() {
                return Err(AuthError::IdentityBind("empty nonce".into()));
            }
            Ok(DcIdentityMessage::Challenge(DcIdentityChallenge { nonce }))
        }
        "dc_response" => {
            let mac_hex = json_string_field(trimmed, "mac")
                .ok_or_else(|| AuthError::IdentityBind("missing mac".into()))?;
            let mac_bytes = hex_to_bytes(&mac_hex)
                .map_err(|e| AuthError::IdentityBind(format!("mac hex: {e}")))?;
            if mac_bytes.len() != DC_BIND_MAC_LEN {
                return Err(AuthError::IdentityBind(format!(
                    "mac must be {DC_BIND_MAC_LEN} bytes, got {}",
                    mac_bytes.len()
                )));
            }
            let mut mac = [0u8; DC_BIND_MAC_LEN];
            mac.copy_from_slice(&mac_bytes);
            Ok(DcIdentityMessage::Response(DcIdentityResponse { mac }))
        }
        other => Err(AuthError::IdentityBind(format!("unknown type `{other}`"))),
    }
}

/// Extract a JSON string field value (simple scanner; not a full JSON parser).
fn json_string_field(obj: &str, key: &str) -> Option<String> {
    let pattern = format!(r#""{key}""#);
    let idx = obj.find(&pattern)?;
    let after_key = &obj[idx + pattern.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let rest = &after_colon[1..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

/// Encode transcript for the DC identity bind MAC.
///
/// Layout: `domain || len||session_id || len||nonce || len||fp_host || len||fp_viewer`
fn encode_dc_bind_message(
    session_id: &[u8],
    nonce: &[u8],
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        DC_BIND_DOMAIN.len()
            + 16
            + session_id.len()
            + nonce.len()
            + fingerprint_host.len()
            + fingerprint_viewer.len(),
    );
    out.extend_from_slice(DC_BIND_DOMAIN);
    append_lp(&mut out, session_id);
    append_lp(&mut out, nonce);
    append_lp(&mut out, fingerprint_host);
    append_lp(&mut out, fingerprint_viewer);
    out
}

fn append_lp(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("field length fits u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

/// Compute the DC identity bind MAC.
pub fn compute_dc_bind_mac(
    bind_key: &SessionBindKey,
    session_id: &[u8],
    nonce: &[u8],
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
) -> [u8; DC_BIND_MAC_LEN] {
    let msg = encode_dc_bind_message(session_id, nonce, fingerprint_host, fingerprint_viewer);
    let mut mac =
        HmacSha256::new_from_slice(bind_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(&msg);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; DC_BIND_MAC_LEN];
    out.copy_from_slice(&result);
    out
}

/// Verify a DC identity bind MAC in constant time.
pub fn verify_dc_bind_mac(
    bind_key: &SessionBindKey,
    session_id: &[u8],
    nonce: &[u8],
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
    presented: &[u8],
) -> Result<()> {
    if presented.len() != DC_BIND_MAC_LEN {
        return Err(AuthError::IdentityBind(format!(
            "mac length {}, expected {DC_BIND_MAC_LEN}",
            presented.len()
        )));
    }
    let expected = compute_dc_bind_mac(
        bind_key,
        session_id,
        nonce,
        fingerprint_host,
        fingerprint_viewer,
    );
    if bool::from(expected.ct_eq(presented)) {
        Ok(())
    } else {
        Err(AuthError::IdentityBind("mac mismatch".into()))
    }
}

/// Viewer-side: build a DC identity response for a received challenge.
pub fn respond_dc_challenge(
    bind_key: &SessionBindKey,
    session_id: &str,
    challenge: &DcIdentityChallenge,
    fingerprint_host: &str,
    fingerprint_viewer: &str,
) -> DcIdentityResponse {
    let mac = compute_dc_bind_mac(
        bind_key,
        session_id.as_bytes(),
        &challenge.nonce,
        fingerprint_host.as_bytes(),
        fingerprint_viewer.as_bytes(),
    );
    DcIdentityResponse { mac }
}

/// Host-side: verify viewer DC response and mark identity bound on success.
pub fn complete_dc_bind(
    state: &mut IdentityBindState,
    bind_key: &SessionBindKey,
    challenge: &DcIdentityChallenge,
    response: &DcIdentityResponse,
    fingerprint_host: &str,
    fingerprint_viewer: &str,
) -> Result<()> {
    verify_dc_bind_mac(
        bind_key,
        state.session_id.as_bytes(),
        &challenge.nonce,
        fingerprint_host.as_bytes(),
        fingerprint_viewer.as_bytes(),
        &response.mac,
    )?;
    state.mark_identity_bound();
    Ok(())
}

// ---------------------------------------------------------------------------
// Mode A / Mode B host verification hooks
// ---------------------------------------------------------------------------

/// Mode A (OTP): verify plaintext code against a host-side record and consume once.
///
/// On success, returns a [`SessionBindKey`] for the post-DTLS DC challenge.
/// The bind key is derived from **OTP digits alone** (public domain salt);
/// `pepper` is used only for OTP hash verification/storage, not the bind key.
pub fn authorize_mode_a(
    record: &mut OtpRecord,
    code: &str,
    pepper: &[u8],
    now_unix: u64,
) -> Result<SessionBindKey> {
    record.verify_and_consume(code, pepper, now_unix)?;
    SessionBindKey::from_mode_a_otp(code)
}

/// Mode B (unattended): verify challenge-response MAC with host-only secret.
///
/// On success, returns a [`SessionBindKey`] for the post-DTLS DC challenge.
///
/// # Fingerprints
///
/// At signaling time, DTLS fingerprints may not yet be known. Callers may pass
/// empty fingerprints during the pre-DTLS Mode B auth exchange; the **DC**
/// bind later re-binds the real `fp_host` / `fp_viewer` values.
pub fn authorize_mode_b(
    secret: &HostSecret,
    challenge: &AuthChallenge,
    session_id: &str,
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
    mac: &[u8],
) -> Result<SessionBindKey> {
    challenge.verify_response(
        secret,
        session_id.as_bytes(),
        fingerprint_host,
        fingerprint_viewer,
        mac,
    )?;
    Ok(SessionBindKey::from_mode_b_secret(secret))
}

/// Viewer helper for Mode B: compute signaling auth_response MAC.
pub fn mode_b_viewer_response(
    secret: &HostSecret,
    session_id: &str,
    nonce: &[u8],
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
) -> [u8; CHALLENGE_MAC_LEN] {
    respond_to_challenge(
        secret,
        session_id.as_bytes(),
        nonce,
        fingerprint_host,
        fingerprint_viewer,
    )
}

/// Re-export for Mode B transcript construction in tests / advanced callers.
pub fn mode_b_verify_mac(
    secret: &HostSecret,
    transcript: &ChallengeTranscript<'_>,
    presented: &[u8],
) -> Result<()> {
    verify_challenge_mac(secret, transcript, presented)
}

// ---------------------------------------------------------------------------
// Hex helpers (lowercase)
// ---------------------------------------------------------------------------

/// Encode bytes as lowercase hex.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Decode lowercase or uppercase hex into bytes.
pub fn hex_to_bytes(hex: &str) -> std::result::Result<Vec<u8>, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> std::result::Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit {}", c as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::generate_device_keypair;
    use crate::otp::mint_otp_record;

    const FP_HOST: &str =
        "sha-256 01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF";
    const FP_VIEWER: &str =
        "sha-256 FE:DC:BA:98:76:54:32:10:FE:DC:BA:98:76:54:32:10:FE:DC:BA:98:76:54:32:10:FE:DC:BA:98:76:54:32:10";

    #[test]
    fn fingerprint_sig_roundtrip() {
        let (sk, vk) = generate_device_keypair();
        let sid = "sess-abc";
        let sig = sign_session_fingerprint(&sk, sid, FP_HOST);
        assert_eq!(sig.len(), 128);
        verify_session_fingerprint(&vk, sid, FP_HOST, &sig).unwrap();
    }

    #[test]
    fn wrong_fingerprint_sig_fails() {
        let (sk, vk) = generate_device_keypair();
        let sig = sign_session_fingerprint(&sk, "sess", FP_HOST);
        assert!(matches!(
            verify_session_fingerprint(&vk, "sess", FP_VIEWER, &sig),
            Err(AuthError::FingerprintSigInvalid)
        ));
        assert!(matches!(
            verify_session_fingerprint(&vk, "other", FP_HOST, &sig),
            Err(AuthError::FingerprintSigInvalid)
        ));
    }

    #[test]
    fn input_allowed_requires_both_flags() {
        let mut s = IdentityBindState::new("s1");
        assert!(!s.input_allowed());
        s.mark_authorized();
        assert!(!s.input_allowed());
        s.mark_identity_bound();
        assert!(s.input_allowed());
        s.reset_flags();
        assert!(!s.input_allowed());
    }

    #[test]
    fn dc_bind_roundtrip() {
        let pepper = b"pepper-material-for-otp!!";
        let (otp, mut rec) = mint_otp_record(6, pepper, u64::MAX).unwrap();
        let bind_key = authorize_mode_a(&mut rec, otp.as_str(), pepper, 0).unwrap();
        // Viewer derives the same key from OTP digits alone (no pepper).
        let viewer_key = SessionBindKey::from_mode_a_otp(otp.as_str()).unwrap();
        assert_eq!(bind_key.as_bytes(), viewer_key.as_bytes());

        let mut state = IdentityBindState::new("sess-dc");
        state.mark_authorized();
        assert!(!state.input_allowed());

        let challenge = DcIdentityChallenge::issue();
        let response = respond_dc_challenge(&viewer_key, "sess-dc", &challenge, FP_HOST, FP_VIEWER);
        complete_dc_bind(
            &mut state, &bind_key, &challenge, &response, FP_HOST, FP_VIEWER,
        )
        .unwrap();
        assert!(state.identity_bound);
        assert!(state.input_allowed());
    }

    #[test]
    fn mode_a_bind_key_independent_of_storage_pepper() {
        let otp = "123456";
        let k1 = SessionBindKey::from_mode_a_otp(otp).unwrap();
        let k2 = SessionBindKey::from_mode_a_otp(otp).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
        // Different OTP → different key.
        let k3 = SessionBindKey::from_mode_a_otp("654321").unwrap();
        assert_ne!(k1.as_bytes(), k3.as_bytes());
        // Invalid format rejected.
        assert!(SessionBindKey::from_mode_a_otp("12").is_err());
    }

    #[test]
    fn dc_bind_wrong_fingerprint_fails() {
        let secret = HostSecret::generate();
        let bind_key = SessionBindKey::from_mode_b_secret(&secret);
        let mut state = IdentityBindState::new("s");
        state.mark_authorized();
        let challenge = DcIdentityChallenge::issue();
        let response = respond_dc_challenge(&bind_key, "s", &challenge, FP_HOST, FP_VIEWER);
        let err = complete_dc_bind(
            &mut state,
            &bind_key,
            &challenge,
            &response,
            FP_HOST,
            "sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99",
        )
        .unwrap_err();
        assert!(matches!(err, AuthError::IdentityBind(_)));
        assert!(!state.identity_bound);
        assert!(!state.input_allowed());
    }

    #[test]
    fn dc_message_encode_parse() {
        let c = DcIdentityChallenge {
            nonce: vec![0xab; 32],
        };
        let parsed = DcIdentityMessage::parse(&c.encode()).unwrap();
        match parsed {
            DcIdentityMessage::Challenge(ch) => assert_eq!(ch.nonce, c.nonce),
            _ => panic!("expected challenge"),
        }

        let r = DcIdentityResponse { mac: [0xcd; 32] };
        let parsed = DcIdentityMessage::parse(&r.encode()).unwrap();
        match parsed {
            DcIdentityMessage::Response(resp) => assert_eq!(resp.mac, r.mac),
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn mode_b_authorize_and_dc_bind() {
        let secret = HostSecret::try_new(b"host-local-secret!!".to_vec()).unwrap();
        let challenge = AuthChallenge::issue();
        let sid = "sess-mode-b";
        // Pre-DTLS: empty fingerprints on signaling CR is allowed.
        let mac = mode_b_viewer_response(&secret, sid, challenge.nonce.as_bytes(), b"", b"");
        let bind_key = authorize_mode_b(&secret, &challenge, sid, b"", b"", &mac).unwrap();

        let mut state = IdentityBindState::new(sid);
        state.mark_authorized();

        let dc = DcIdentityChallenge::issue();
        let resp = respond_dc_challenge(&bind_key, sid, &dc, FP_HOST, FP_VIEWER);
        complete_dc_bind(&mut state, &bind_key, &dc, &resp, FP_HOST, FP_VIEWER).unwrap();
        assert!(state.input_allowed());
    }

    #[test]
    fn mode_b_wrong_mac_fails() {
        let secret = HostSecret::generate();
        let challenge = AuthChallenge::issue();
        let bad = [0u8; 32];
        assert!(matches!(
            authorize_mode_b(&secret, &challenge, "s", b"h", b"v", &bad),
            Err(AuthError::ChallengeMacMismatch)
        ));
    }

    #[test]
    fn mode_a_double_consume_fails() {
        let pepper = b"p".repeat(16);
        let (otp, mut rec) = mint_otp_record(6, &pepper, u64::MAX).unwrap();
        authorize_mode_a(&mut rec, otp.as_str(), &pepper, 0).unwrap();
        assert!(authorize_mode_a(&mut rec, otp.as_str(), &pepper, 0).is_err());
    }

    #[test]
    fn no_input_before_bind_gate() {
        let s = IdentityBindState::new("x");
        assert!(matches!(
            s.input_gate_error(),
            AuthError::InputNotAllowed {
                identity_bound: false,
                session_authorized: false,
            }
        ));
    }

    #[test]
    fn bind_key_debug_redacts() {
        let k = SessionBindKey::try_new(b"secret-material").unwrap();
        assert!(format!("{k:?}").contains("redacted"));
    }
}
