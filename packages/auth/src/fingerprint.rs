//! Fingerprint binding: sign/verify `session_id` + DTLS fingerprint with ed25519.

use crate::error::{AuthError, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

/// Domain separator for fingerprint binding messages.
const BIND_DOMAIN: &[u8] = b"remotelink-fingerprint-bind-v1";

/// Generate a new ed25519 device keypair.
pub fn generate_device_keypair() -> (SigningKey, VerifyingKey) {
    let signing = SigningKey::generate(&mut OsRng);
    let verifying = signing.verifying_key();
    (signing, verifying)
}

/// Encode the signed payload: domain || len||session_id || len||fingerprint.
pub fn encode_binding_message(session_id: &[u8], fingerprint: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIND_DOMAIN.len() + 8 + session_id.len() + fingerprint.len());
    out.extend_from_slice(BIND_DOMAIN);
    let sl = u32::try_from(session_id.len()).expect("session_id length fits u32");
    let fl = u32::try_from(fingerprint.len()).expect("fingerprint length fits u32");
    out.extend_from_slice(&sl.to_be_bytes());
    out.extend_from_slice(session_id);
    out.extend_from_slice(&fl.to_be_bytes());
    out.extend_from_slice(fingerprint);
    out
}

/// Sign `session_id` + DTLS fingerprint with the enrolled device key.
pub fn sign_fingerprint_binding(
    signing_key: &SigningKey,
    session_id: &[u8],
    fingerprint: &[u8],
) -> [u8; 64] {
    let msg = encode_binding_message(session_id, fingerprint);
    let sig = signing_key.sign(&msg);
    sig.to_bytes()
}

/// Verify a fingerprint binding signature.
pub fn verify_fingerprint_binding(
    verifying_key: &VerifyingKey,
    session_id: &[u8],
    fingerprint: &[u8],
    signature: &[u8],
) -> Result<()> {
    let sig_bytes: [u8; 64] = signature
        .try_into()
        .map_err(|_| AuthError::FingerprintSigInvalid)?;
    let sig = Signature::from_bytes(&sig_bytes);
    let msg = encode_binding_message(session_id, fingerprint);
    verifying_key
        .verify(&msg, &sig)
        .map_err(|_| AuthError::FingerprintSigInvalid)
}

/// Reconstruct a verifying key from 32 raw bytes.
pub fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthError::Crypto("verifying key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| AuthError::Crypto(e.to_string()))
}

/// Reconstruct a signing key from 32 raw seed bytes.
pub fn signing_key_from_bytes(bytes: &[u8]) -> Result<SigningKey> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthError::Crypto("signing key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_roundtrip() {
        let (sk, vk) = generate_device_keypair();
        let session = b"550e8400-e29b-41d4-a716-446655440000";
        let fp = b"https://example/fp/sha-256 AA:BB:CC";
        let sig = sign_fingerprint_binding(&sk, session, fp);
        assert_eq!(sig.len(), 64);
        verify_fingerprint_binding(&vk, session, fp, &sig).unwrap();
    }

    #[test]
    fn tampered_session_fails() {
        let (sk, vk) = generate_device_keypair();
        let sig = sign_fingerprint_binding(&sk, b"sess-a", b"fp");
        assert!(matches!(
            verify_fingerprint_binding(&vk, b"sess-b", b"fp", &sig),
            Err(AuthError::FingerprintSigInvalid)
        ));
    }

    #[test]
    fn tampered_fingerprint_fails() {
        let (sk, vk) = generate_device_keypair();
        let sig = sign_fingerprint_binding(&sk, b"sess", b"fp-a");
        assert!(matches!(
            verify_fingerprint_binding(&vk, b"sess", b"fp-b", &sig),
            Err(AuthError::FingerprintSigInvalid)
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let (sk, _) = generate_device_keypair();
        let (_, vk2) = generate_device_keypair();
        let sig = sign_fingerprint_binding(&sk, b"s", b"f");
        assert!(verify_fingerprint_binding(&vk2, b"s", b"f", &sig).is_err());
    }

    #[test]
    fn bad_signature_length() {
        let (_, vk) = generate_device_keypair();
        assert!(matches!(
            verify_fingerprint_binding(&vk, b"s", b"f", &[0u8; 10]),
            Err(AuthError::FingerprintSigInvalid)
        ));
    }

    #[test]
    fn key_bytes_roundtrip() {
        let (sk, vk) = generate_device_keypair();
        let sk2 = signing_key_from_bytes(sk.as_bytes()).unwrap();
        let vk2 = verifying_key_from_bytes(vk.as_bytes()).unwrap();
        let sig = sign_fingerprint_binding(&sk2, b"id", b"fp");
        verify_fingerprint_binding(&vk2, b"id", b"fp", &sig).unwrap();
    }

    #[test]
    fn key_bytes_wrong_len() {
        assert!(signing_key_from_bytes(&[0u8; 16]).is_err());
        assert!(verifying_key_from_bytes(&[0u8; 16]).is_err());
    }

    #[test]
    fn encode_stable_and_domain() {
        let m = encode_binding_message(b"a", b"b");
        assert_eq!(m, encode_binding_message(b"a", b"b"));
        assert!(m.windows(BIND_DOMAIN.len()).any(|w| w == BIND_DOMAIN));
    }
}
