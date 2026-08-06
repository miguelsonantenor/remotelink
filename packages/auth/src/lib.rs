//! RemoteLink authentication helpers (PR 3 / KD8, KD14, KD17).
//!
//! # Modules
//!
//! - [`device_id`] — numeric public device IDs + Luhn check digits (KD14)
//! - [`otp`] — Mode A ad-hoc OTP generate / hash / verify / consume-once
//! - [`challenge`] — Mode B host-only secret challenge-response (HMAC-SHA256)
//! - [`password`] — Mode C Argon2id hash + verify (server pre-filter)
//! - [`fingerprint`] — ed25519 sign/verify of session_id + DTLS fingerprint
//!
//! Re-exports common entry points at the crate root for convenience.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod challenge;
pub mod device_id;
pub mod error;
pub mod fingerprint;
pub mod otp;
pub mod password;

pub use challenge::{
    compute_challenge_mac, respond_to_challenge, verify_challenge_mac, AuthChallenge,
    ChallengeNonce, ChallengeTranscript, HostSecret, CHALLENGE_MAC_LEN, CHALLENGE_NONCE_LEN,
    HOST_SECRET_MIN_LEN,
};
pub use device_id::{
    luhn_check_digit, validate_luhn, DevicePublicId, DEVICE_ID_BODY_DIGITS, DEVICE_ID_TOTAL_DIGITS,
};
pub use error::{AuthError, Result};
pub use fingerprint::{
    encode_binding_message, generate_device_keypair, sign_fingerprint_binding,
    signing_key_from_bytes, verify_fingerprint_binding, verifying_key_from_bytes,
};
pub use otp::{
    generate_otp, generate_otp_default, generate_otp_with_rng, hash_otp, hash_otp_unkeyed,
    hash_otp_with_salt, mint_otp, mint_otp_record, validate_otp_format, verify_otp, OtpCode,
    OtpHash, OtpRecord, OTP_DEFAULT_DIGITS, OTP_MAX_DIGITS, OTP_MIN_DIGITS, OTP_SALT_LEN,
};
pub use password::{hash_password, password_matches, verify_password};

/// Crate version (mirrors workspace package version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_nonempty() {
        assert!(!VERSION.is_empty());
        assert_eq!(VERSION, remotelink_common::VERSION);
    }

    #[test]
    fn end_to_end_auth_sketch() {
        let pepper = b"demo-otp-pepper-material!";

        // Device ID for call-out
        let public_id = DevicePublicId::generate();
        assert!(DevicePublicId::is_valid(public_id.as_str()));

        // Mode A OTP mint (keyed)
        let (otp, mut rec) = mint_otp_record(6, pepper, u64::MAX).unwrap();
        rec.verify_and_consume(otp.as_str(), pepper, 0).unwrap();

        // Mode B challenge-response
        let secret = HostSecret::generate();
        let ch = AuthChallenge::issue();
        let mac = respond_to_challenge(
            &secret,
            b"session-1",
            ch.nonce.as_bytes(),
            b"fp-host",
            b"fp-viewer",
        );
        ch.verify_response(&secret, b"session-1", b"fp-host", b"fp-viewer", &mac)
            .unwrap();

        // Mode C password prefilter
        let hash = hash_password(b"enterprise-pw").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(password_matches(b"enterprise-pw", &hash));

        // Fingerprint binding
        let (sk, vk) = generate_device_keypair();
        let sig = sign_fingerprint_binding(&sk, b"session-1", b"fp-host");
        verify_fingerprint_binding(&vk, b"session-1", b"fp-host", &sig).unwrap();
    }
}
