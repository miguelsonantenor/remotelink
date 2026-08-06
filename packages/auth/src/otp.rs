//! Mode A: ad-hoc OTP generation, hashing, verification, and consume-once.

use crate::error::{AuthError, Result};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Minimum OTP digit length (inclusive).
pub const OTP_MIN_DIGITS: usize = 6;
/// Maximum OTP digit length (inclusive).
pub const OTP_MAX_DIGITS: usize = 8;
/// Default OTP digit length.
pub const OTP_DEFAULT_DIGITS: usize = 6;

/// Domain separation prefix for OTP hashes (prevents cross-protocol reuse).
const OTP_HASH_DOMAIN: &[u8] = b"remotelink-otp-v1:";

/// Plaintext OTP code (zeroized on drop).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct OtpCode {
    digits: String,
}

impl OtpCode {
    /// Borrow the digit string.
    pub fn as_str(&self) -> &str {
        &self.digits
    }

    /// Number of digits.
    pub fn len(&self) -> usize {
        self.digits.len()
    }

    /// Always false for a successfully constructed code (satisfies clippy).
    pub fn is_empty(&self) -> bool {
        self.digits.is_empty()
    }
}

impl std::fmt::Debug for OtpCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtpCode([redacted])")
    }
}

impl std::fmt::Display for OtpCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.digits)
    }
}

/// SHA-256 hash of an OTP (store this, never the plaintext).
#[derive(Clone, PartialEq, Eq)]
pub struct OtpHash {
    /// 32-byte SHA-256 digest.
    pub digest: [u8; 32],
}

impl std::fmt::Debug for OtpHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtpHash").field("digest", &"...").finish()
    }
}

/// In-memory OTP record supporting single-use consumption and expiry.
#[derive(Debug, Clone)]
pub struct OtpRecord {
    hash: OtpHash,
    expires_at_unix: u64,
    consumed: bool,
}

impl OtpRecord {
    /// Create a new unconsumed record.
    pub fn new(hash: OtpHash, expires_at_unix: u64) -> Self {
        Self {
            hash,
            expires_at_unix,
            consumed: false,
        }
    }

    /// Stored hash.
    pub fn hash(&self) -> &OtpHash {
        &self.hash
    }

    /// Expiry as unix seconds.
    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    /// Whether the OTP has already been consumed.
    pub fn is_consumed(&self) -> bool {
        self.consumed
    }

    /// Whether the OTP is expired at `now_unix`.
    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at_unix
    }

    /// Verify plaintext against this record without consuming.
    pub fn verify(&self, code: &str, now_unix: u64) -> Result<()> {
        if self.consumed {
            return Err(AuthError::OtpVerify("already consumed".into()));
        }
        if self.is_expired(now_unix) {
            return Err(AuthError::OtpVerify("expired".into()));
        }
        let candidate = hash_otp(code)?;
        if !bool::from(candidate.digest.ct_eq(&self.hash.digest)) {
            return Err(AuthError::OtpVerify("code mismatch".into()));
        }
        Ok(())
    }

    /// Verify and mark consumed on success (consume-once).
    ///
    /// A second successful verification attempt returns an error.
    pub fn verify_and_consume(&mut self, code: &str, now_unix: u64) -> Result<()> {
        self.verify(code, now_unix)?;
        self.consumed = true;
        Ok(())
    }

    /// Mark consumed without verification (e.g. after host-side accept path).
    pub fn mark_consumed(&mut self) {
        self.consumed = true;
    }
}

/// Generate a numeric OTP with `digits` length in \[`OTP_MIN_DIGITS`, `OTP_MAX_DIGITS`\].
pub fn generate_otp(digits: usize) -> Result<OtpCode> {
    let mut rng = rand::thread_rng();
    generate_otp_with_rng(digits, &mut rng)
}

/// Generate OTP using a provided RNG.
pub fn generate_otp_with_rng<R: Rng + ?Sized>(digits: usize, rng: &mut R) -> Result<OtpCode> {
    if !(OTP_MIN_DIGITS..=OTP_MAX_DIGITS).contains(&digits) {
        return Err(AuthError::InvalidOtpFormat(format!(
            "digits must be {OTP_MIN_DIGITS}..={OTP_MAX_DIGITS}, got {digits}"
        )));
    }
    let mut s = String::with_capacity(digits);
    for _ in 0..digits {
        s.push(char::from(b'0' + rng.gen_range(0..10u8)));
    }
    Ok(OtpCode { digits: s })
}

/// Generate a default-length (6-digit) OTP.
pub fn generate_otp_default() -> OtpCode {
    generate_otp(OTP_DEFAULT_DIGITS).expect("default digit count is valid")
}

/// Validate OTP plaintext format (digit count and numeric).
pub fn validate_otp_format(code: &str) -> Result<()> {
    let len = code.len();
    if !(OTP_MIN_DIGITS..=OTP_MAX_DIGITS).contains(&len) {
        return Err(AuthError::InvalidOtpFormat(format!(
            "length must be {OTP_MIN_DIGITS}..={OTP_MAX_DIGITS}, got {len}"
        )));
    }
    if !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(AuthError::InvalidOtpFormat(
            "OTP must be numeric digits only".into(),
        ));
    }
    Ok(())
}

/// Hash an OTP for storage (SHA-256 with domain separation).
pub fn hash_otp(code: &str) -> Result<OtpHash> {
    validate_otp_format(code)?;
    let mut hasher = Sha256::new();
    hasher.update(OTP_HASH_DOMAIN);
    hasher.update(code.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(OtpHash { digest })
}

/// Verify a plaintext code against a stored hash (constant-time compare).
pub fn verify_otp(code: &str, expected: &OtpHash) -> Result<()> {
    let candidate = hash_otp(code)?;
    if bool::from(candidate.digest.ct_eq(&expected.digest)) {
        Ok(())
    } else {
        Err(AuthError::OtpVerify("code mismatch".into()))
    }
}

/// Convenience: generate OTP + hash pair for minting.
pub fn mint_otp(digits: usize) -> Result<(OtpCode, OtpHash)> {
    let code = generate_otp(digits)?;
    let hash = hash_otp(code.as_str())?;
    Ok((code, hash))
}

/// Mint with explicit expiry unix timestamp.
pub fn mint_otp_record(digits: usize, expires_at_unix: u64) -> Result<(OtpCode, OtpRecord)> {
    let (code, hash) = mint_otp(digits)?;
    Ok((code, OtpRecord::new(hash, expires_at_unix)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn generate_default_is_six_digits() {
        let code = generate_otp_default();
        assert_eq!(code.len(), 6);
        assert!(!code.is_empty());
        assert!(code.as_str().chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn generate_rejects_out_of_range() {
        assert!(matches!(
            generate_otp(5),
            Err(AuthError::InvalidOtpFormat(_))
        ));
        assert!(matches!(
            generate_otp(9),
            Err(AuthError::InvalidOtpFormat(_))
        ));
    }

    #[test]
    fn generate_6_7_8_ok() {
        for n in 6..=8 {
            let code = generate_otp(n).unwrap();
            assert_eq!(code.len(), n);
        }
    }

    #[test]
    fn generate_with_rng_deterministic() {
        let mut a = StdRng::seed_from_u64(7);
        let mut b = StdRng::seed_from_u64(7);
        let ca = generate_otp_with_rng(6, &mut a).unwrap();
        let cb = generate_otp_with_rng(6, &mut b).unwrap();
        assert_eq!(ca.as_str(), cb.as_str());
    }

    #[test]
    fn hash_and_verify_roundtrip() {
        let code = generate_otp(8).unwrap();
        let hash = hash_otp(code.as_str()).unwrap();
        assert!(verify_otp(code.as_str(), &hash).is_ok());
        assert!(verify_otp("00000000", &hash).is_err());
    }

    #[test]
    fn hash_is_domain_separated() {
        // Same digits without domain would differ — we only check stability.
        let h1 = hash_otp("123456").unwrap();
        let h2 = hash_otp("123456").unwrap();
        assert_eq!(h1.digest, h2.digest);
        let h3 = hash_otp("123457").unwrap();
        assert_ne!(h1.digest, h3.digest);
    }

    #[test]
    fn validate_format_errors() {
        assert!(validate_otp_format("12345").is_err());
        assert!(validate_otp_format("123456789").is_err());
        assert!(validate_otp_format("12a456").is_err());
        assert!(validate_otp_format("123456").is_ok());
    }

    #[test]
    fn record_verify_and_consume_once() {
        let (code, mut rec) = mint_otp_record(6, 1_700_000_100).unwrap();
        assert!(!rec.is_consumed());
        assert!(!rec.is_expired(1_700_000_000));
        assert!(rec.is_expired(1_700_000_100));

        // Expired
        assert!(matches!(
            rec.verify(code.as_str(), 1_700_000_100),
            Err(AuthError::OtpVerify(_))
        ));

        // Fresh verify
        rec.verify(code.as_str(), 1_700_000_000).unwrap();
        rec.verify_and_consume(code.as_str(), 1_700_000_000)
            .unwrap();
        assert!(rec.is_consumed());

        // Second consume fails
        assert!(matches!(
            rec.verify_and_consume(code.as_str(), 1_700_000_000),
            Err(AuthError::OtpVerify(_))
        ));
    }

    #[test]
    fn record_wrong_code() {
        let (_code, rec) = mint_otp_record(6, u64::MAX).unwrap();
        assert!(rec.verify("000000", 0).is_err());
    }

    #[test]
    fn mark_consumed_blocks_verify() {
        let (code, mut rec) = mint_otp_record(6, u64::MAX).unwrap();
        rec.mark_consumed();
        assert!(rec.verify(code.as_str(), 0).is_err());
    }

    #[test]
    fn mint_otp_pair() {
        let (code, hash) = mint_otp(7).unwrap();
        assert_eq!(code.len(), 7);
        verify_otp(code.as_str(), &hash).unwrap();
    }

    #[test]
    fn otp_debug_redacts() {
        let code = generate_otp_default();
        let s = format!("{code:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains(code.as_str()));
        // Display shows digits (for host UI)
        assert_eq!(format!("{code}"), code.as_str());
    }

    #[test]
    fn otp_hash_debug() {
        let h = hash_otp("123456").unwrap();
        let s = format!("{h:?}");
        assert!(s.contains("OtpHash"));
        assert_eq!(h, h.clone());
    }

    #[test]
    fn record_accessors() {
        let hash = hash_otp("654321").unwrap();
        let rec = OtpRecord::new(hash.clone(), 99);
        assert_eq!(rec.hash().digest, hash.digest);
        assert_eq!(rec.expires_at_unix(), 99);
    }
}
