//! Mode B: unattended challenge-response helpers (HMAC-SHA256 MAC path).
//!
//! Host holds secret `K` locally only. Viewer proves knowledge via:
//! `MAC_K(session_id || nonce || fingerprint_host || fingerprint_viewer)`.
//!
//! Full PAKE (e.g. SPAKE2+) is intentionally stubbed; see [`pake`].

use crate::error::{AuthError, Result};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// Recommended nonce length in bytes.
pub const CHALLENGE_NONCE_LEN: usize = 32;

/// Length of the MAC tag (SHA-256).
pub const CHALLENGE_MAC_LEN: usize = 32;

/// Minimum accepted host secret length (bytes). Callers must supply
/// high-entropy material for the non-PAKE MAC path.
pub const HOST_SECRET_MIN_LEN: usize = 16;

/// Domain separator for the MAC message.
const MAC_DOMAIN: &[u8] = b"remotelink-mode-b-mac-v1";

/// Host-only unattended secret (zeroized on drop).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct HostSecret {
    bytes: Vec<u8>,
}

impl HostSecret {
    /// Wrap raw secret bytes; rejects empty/short material.
    ///
    /// Prefer [`HostSecret::generate`] for fresh high-entropy secrets.
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.len() < HOST_SECRET_MIN_LEN {
            return Err(AuthError::Crypto(format!(
                "host secret must be at least {HOST_SECRET_MIN_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self { bytes })
    }

    /// Generate a random 32-byte secret from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self { bytes }
    }

    /// Borrow secret bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for HostSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostSecret([redacted])")
    }
}

/// Random challenge nonce issued by the host.
#[derive(Clone, PartialEq, Eq)]
pub struct ChallengeNonce {
    /// Raw nonce bytes.
    pub bytes: Vec<u8>,
}

impl ChallengeNonce {
    /// Generate a [`CHALLENGE_NONCE_LEN`]-byte nonce from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; CHALLENGE_NONCE_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self { bytes }
    }

    /// Construct from existing bytes (must be non-empty).
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(AuthError::Crypto("nonce must be non-empty".into()));
        }
        Ok(Self { bytes })
    }

    /// Borrow nonce bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for ChallengeNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengeNonce")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// Inputs bound into the Mode B MAC.
#[derive(Debug, Clone)]
pub struct ChallengeTranscript<'a> {
    /// Session identifier (opaque string/UUID bytes as UTF-8 or raw).
    pub session_id: &'a [u8],
    /// Host-issued nonce.
    pub nonce: &'a [u8],
    /// Host DTLS certificate fingerprint (raw or hex-decoded).
    pub fingerprint_host: &'a [u8],
    /// Viewer DTLS certificate fingerprint.
    pub fingerprint_viewer: &'a [u8],
}

impl ChallengeTranscript<'_> {
    /// Encode transcript as length-prefixed fields under a domain separator.
    ///
    /// Layout: `domain || len||session_id || len||nonce || len||fp_host || len||fp_viewer`
    /// where `len` is a big-endian u32.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            MAC_DOMAIN.len()
                + 4 * 4
                + self.session_id.len()
                + self.nonce.len()
                + self.fingerprint_host.len()
                + self.fingerprint_viewer.len(),
        );
        out.extend_from_slice(MAC_DOMAIN);
        append_lp(&mut out, self.session_id);
        append_lp(&mut out, self.nonce);
        append_lp(&mut out, self.fingerprint_host);
        append_lp(&mut out, self.fingerprint_viewer);
        out
    }
}

fn append_lp(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("field length fits u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

/// Compute `HMAC-SHA256_K(transcript)` for Mode B.
pub fn compute_challenge_mac(
    secret: &HostSecret,
    transcript: &ChallengeTranscript<'_>,
) -> [u8; 32] {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(&transcript.encode());
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Verify a Mode B MAC in constant time.
pub fn verify_challenge_mac(
    secret: &HostSecret,
    transcript: &ChallengeTranscript<'_>,
    presented: &[u8],
) -> Result<()> {
    if presented.len() != CHALLENGE_MAC_LEN {
        return Err(AuthError::ChallengeMacMismatch);
    }
    let expected = compute_challenge_mac(secret, transcript);
    if bool::from(expected.ct_eq(presented)) {
        Ok(())
    } else {
        Err(AuthError::ChallengeMacMismatch)
    }
}

/// Full host-side challenge issue + later verify helpers.
#[derive(Debug, Clone)]
pub struct AuthChallenge {
    /// Nonce sent to the viewer.
    pub nonce: ChallengeNonce,
}

impl AuthChallenge {
    /// Issue a fresh challenge (host → viewer).
    pub fn issue() -> Self {
        Self {
            nonce: ChallengeNonce::generate(),
        }
    }

    /// Verify viewer response MAC.
    pub fn verify_response(
        &self,
        secret: &HostSecret,
        session_id: &[u8],
        fingerprint_host: &[u8],
        fingerprint_viewer: &[u8],
        mac: &[u8],
    ) -> Result<()> {
        let transcript = ChallengeTranscript {
            session_id,
            nonce: self.nonce.as_bytes(),
            fingerprint_host,
            fingerprint_viewer,
        };
        verify_challenge_mac(secret, &transcript, mac)
    }
}

/// Viewer-side helper: build MAC for a received challenge.
pub fn respond_to_challenge(
    secret: &HostSecret,
    session_id: &[u8],
    nonce: &[u8],
    fingerprint_host: &[u8],
    fingerprint_viewer: &[u8],
) -> [u8; 32] {
    let transcript = ChallengeTranscript {
        session_id,
        nonce,
        fingerprint_host,
        fingerprint_viewer,
    };
    compute_challenge_mac(secret, &transcript)
}

/// PAKE stubs — full SPAKE2+ (or similar) is deferred.
///
/// The MAC path above is the unit-testable Mode B proof for v1 pairing secrets
/// that are already high-entropy. Password-based PAKE will replace/augment this.
pub mod pake {
    /// Placeholder for a future SPAKE2+ (or similar) verifier state.
    #[derive(Debug, Clone, Default)]
    pub struct PakeVerifierStub {
        /// Opaque reserved field for future use.
        pub _reserved: (),
    }

    impl PakeVerifierStub {
        /// Create a stub verifier. **Not a real PAKE.**
        pub fn new() -> Self {
            Self { _reserved: () }
        }

        /// Returns an error until PAKE is implemented.
        pub fn start_handshake(&self, _password: &[u8]) -> Result<(), &'static str> {
            // TODO(auth): implement SPAKE2+ (or chosen PAKE) for low-entropy unattended passwords.
            Err("PAKE not implemented; use Mode B HMAC path for high-entropy host secrets")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_generate_length() {
        let n = ChallengeNonce::generate();
        assert_eq!(n.as_bytes().len(), CHALLENGE_NONCE_LEN);
    }

    #[test]
    fn nonce_from_bytes_rejects_empty() {
        assert!(ChallengeNonce::from_bytes(vec![]).is_err());
        let n = ChallengeNonce::from_bytes(vec![1, 2, 3]).unwrap();
        assert_eq!(n.as_bytes(), &[1, 2, 3]);
    }

    #[test]
    fn host_secret_generate_is_32_bytes() {
        let s = HostSecret::generate();
        assert_eq!(s.as_bytes().len(), 32);
    }

    #[test]
    fn host_secret_try_new_rejects_short() {
        assert!(HostSecret::try_new(vec![]).is_err());
        assert!(HostSecret::try_new(vec![0u8; HOST_SECRET_MIN_LEN - 1]).is_err());
        assert!(HostSecret::try_new(vec![0u8; HOST_SECRET_MIN_LEN]).is_ok());
    }

    #[test]
    fn mac_roundtrip() {
        let secret = HostSecret::try_new(b"host-local-secret-material!!".to_vec()).unwrap();
        let challenge = AuthChallenge::issue();
        let session = b"session-uuid-1234";
        let fp_h = b"sha256 fp host";
        let fp_v = b"sha256 fp viewer";

        let mac = respond_to_challenge(&secret, session, challenge.nonce.as_bytes(), fp_h, fp_v);
        assert_eq!(mac.len(), CHALLENGE_MAC_LEN);
        challenge
            .verify_response(&secret, session, fp_h, fp_v, &mac)
            .unwrap();
    }

    #[test]
    fn mac_mismatch_wrong_secret() {
        let secret = HostSecret::try_new(b"correct-secret!!".to_vec()).unwrap();
        let wrong = HostSecret::try_new(b"wrong-secret!!!!".to_vec()).unwrap();
        let nonce = ChallengeNonce::from_bytes(vec![9; 32]).unwrap();
        let t = ChallengeTranscript {
            session_id: b"s",
            nonce: nonce.as_bytes(),
            fingerprint_host: b"h",
            fingerprint_viewer: b"v",
        };
        let mac = compute_challenge_mac(&secret, &t);
        assert!(verify_challenge_mac(&wrong, &t, &mac).is_err());
        assert!(verify_challenge_mac(&secret, &t, &mac).is_ok());
    }

    #[test]
    fn mac_binds_all_fields() {
        let secret = HostSecret::generate();
        let base = ChallengeTranscript {
            session_id: b"sess",
            nonce: b"nonce-bytes-here-pad-pad-pad!!!!",
            fingerprint_host: b"host-fp",
            fingerprint_viewer: b"viewer-fp",
        };
        let mac = compute_challenge_mac(&secret, &base);

        let mut altered = base.clone();
        altered.session_id = b"SESS";
        assert!(verify_challenge_mac(&secret, &altered, &mac).is_err());

        altered = base.clone();
        altered.nonce = b"nonce-bytes-here-pad-pad-pad!!!?";
        assert!(verify_challenge_mac(&secret, &altered, &mac).is_err());

        altered = base.clone();
        altered.fingerprint_host = b"HOST-fp";
        assert!(verify_challenge_mac(&secret, &altered, &mac).is_err());

        altered = base.clone();
        altered.fingerprint_viewer = b"VIEWER-fp";
        assert!(verify_challenge_mac(&secret, &altered, &mac).is_err());
    }

    #[test]
    fn verify_rejects_wrong_mac_length() {
        let secret = HostSecret::generate();
        let t = ChallengeTranscript {
            session_id: b"s",
            nonce: b"n",
            fingerprint_host: b"h",
            fingerprint_viewer: b"v",
        };
        assert!(verify_challenge_mac(&secret, &t, &[0u8; 16]).is_err());
    }

    #[test]
    fn host_secret_debug_redacts() {
        let s = HostSecret::try_new(b"sekret-material!!".to_vec()).unwrap();
        assert!(format!("{s:?}").contains("redacted"));
        assert_eq!(s.as_bytes(), b"sekret-material!!");
    }

    #[test]
    fn nonce_debug_shows_len_only() {
        let n = ChallengeNonce::from_bytes(vec![0; 8]).unwrap();
        let d = format!("{n:?}");
        assert!(d.contains("len"));
    }

    #[test]
    fn pake_stub_errors() {
        let stub = pake::PakeVerifierStub::new();
        assert!(stub.start_handshake(b"password").is_err());
        let _ = pake::PakeVerifierStub::default();
    }

    #[test]
    fn encode_is_stable() {
        let t = ChallengeTranscript {
            session_id: b"ab",
            nonce: b"cd",
            fingerprint_host: b"ef",
            fingerprint_viewer: b"gh",
        };
        assert_eq!(t.encode(), t.encode());
        assert!(t.encode().starts_with(MAC_DOMAIN));
    }
}
