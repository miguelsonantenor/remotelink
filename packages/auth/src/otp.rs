//! Mode A: ad-hoc OTP generation, hashing, verification, and consume-once.
//!
//! # Hashing security
//!
//! OTP codes are only 6–8 digits (\(10^6\)–\(10^8\) possibilities). A bare
//! SHA-256 of the code is **not offline-hard**: anyone with a stolen digest can
//! brute-force the code instantly. Use [`hash_otp`] / [`verify_otp`] with a
//! server/host **pepper** (never stored in the OTP row) and the per-record
//! **salt** embedded in [`OtpHash`]. Plain domain-separated SHA-256 without a
//! pepper is available only as [`hash_otp_unkeyed`] for integrity/lookup demos
//! and is explicitly **not** recommended for production storage.
//!
//! TTL + consume-once limit online abuse; they do not replace pepper+salt for
//! offline resistance if the hash store leaks.

use crate::error::{AuthError, Result};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::{Rng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// Minimum OTP digit length (inclusive).
pub const OTP_MIN_DIGITS: usize = 6;
/// Maximum OTP digit length (inclusive).
pub const OTP_MAX_DIGITS: usize = 8;
/// Default OTP digit length.
pub const OTP_DEFAULT_DIGITS: usize = 6;
/// Per-OTP salt length in bytes (stored beside the digest).
pub const OTP_SALT_LEN: usize = 16;

/// Domain separation prefix for OTP hashes (prevents cross-protocol reuse).
const OTP_HASH_DOMAIN: &[u8] = b"remotelink-otp-v1:";

/// Plaintext OTP code (zeroized on drop).
///
/// # Display / logging
///
/// This type does **not** implement [`std::fmt::Display`]. The digits are a
/// live secret: use [`OtpCode::as_str`] or [`OtpCode::to_ui_string`] only at
/// intentional host-UI boundaries. Never log or interpolate into error strings.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct OtpCode {
    digits: String,
}

impl OtpCode {
    /// Borrow the digit string (secret-bearing).
    pub fn as_str(&self) -> &str {
        &self.digits
    }

    /// Copy digits for host UI display only. Prefer this over free-form
    /// formatting so call sites are explicit about secret exposure.
    pub fn to_ui_string(&self) -> String {
        self.digits.clone()
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

/// Stored OTP digest + salt.
///
/// Equality is intentionally **not** derived: use [`OtpHash::ct_eq`] or
/// [`verify_otp`] so comparisons stay constant-time.
#[derive(Clone)]
pub struct OtpHash {
    /// 32-byte MAC/digest.
    pub digest: [u8; 32],
    /// Random per-OTP salt (all zeros when produced by [`hash_otp_unkeyed`]).
    pub salt: [u8; OTP_SALT_LEN],
    /// `true` when digest is HMAC-SHA256 under a pepper; `false` for unkeyed SHA-256.
    pub keyed: bool,
}

impl OtpHash {
    /// Constant-time digest (+ salt/flag) equality.
    pub fn ct_eq(&self, other: &Self) -> bool {
        let dig = self.digest.ct_eq(&other.digest);
        let salt = self.salt.ct_eq(&other.salt);
        let keyed = (self.keyed as u8).ct_eq(&(other.keyed as u8));
        bool::from(dig & salt & keyed)
    }
}

impl std::fmt::Debug for OtpHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtpHash")
            .field("digest", &"...")
            .field("salt", &"...")
            .field("keyed", &self.keyed)
            .finish()
    }
}

/// In-memory OTP record supporting single-use consumption and expiry.
///
/// # Consume-once semantics
///
/// Consumption state lives on **this instance only**. This type does **not**
/// implement [`Clone`]: cloning would fork an unconsumed twin and silently
/// allow a second accept of the same OTP at the library layer.
///
/// Durable, distributed consume-once **must** be enforced externally (e.g.
/// Postgres `UPDATE ... WHERE consumed_at IS NULL` CAS per DESIGN). Use this
/// helper for host-local windows or single-process tests only.
#[derive(Debug)]
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
    ///
    /// For keyed hashes, `pepper` must match the value used at mint time.
    /// For unkeyed hashes, pass an empty pepper (`&[]`).
    pub fn verify(&self, code: &str, pepper: &[u8], now_unix: u64) -> Result<()> {
        if self.consumed {
            return Err(AuthError::OtpVerify("already consumed".into()));
        }
        if self.is_expired(now_unix) {
            return Err(AuthError::OtpVerify("expired".into()));
        }
        verify_otp(code, pepper, &self.hash)
    }

    /// Verify and mark consumed on success (consume-once on this instance).
    ///
    /// A second successful verification attempt returns an error.
    pub fn verify_and_consume(&mut self, code: &str, pepper: &[u8], now_unix: u64) -> Result<()> {
        self.verify(code, pepper, now_unix)?;
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
    generate_otp_with_rng(digits, &mut OsRng)
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

fn random_salt() -> [u8; OTP_SALT_LEN] {
    let mut salt = [0u8; OTP_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Hash an OTP with HMAC-SHA256 under a pepper and a random salt (recommended).
///
/// Layout: `HMAC-SHA256(pepper, domain || salt || code)`.
/// Store `digest` + `salt` in the OTP row; keep `pepper` only in host/server config.
pub fn hash_otp(code: &str, pepper: &[u8]) -> Result<OtpHash> {
    if pepper.is_empty() {
        return Err(AuthError::Crypto(
            "OTP pepper must be non-empty for keyed hashing".into(),
        ));
    }
    let salt = random_salt();
    hash_otp_with_salt(code, pepper, &salt)
}

/// Hash an OTP with an explicit salt (for verification recomputation).
pub fn hash_otp_with_salt(code: &str, pepper: &[u8], salt: &[u8]) -> Result<OtpHash> {
    validate_otp_format(code)?;
    if pepper.is_empty() {
        return Err(AuthError::Crypto(
            "OTP pepper must be non-empty for keyed hashing".into(),
        ));
    }
    if salt.len() != OTP_SALT_LEN {
        return Err(AuthError::Crypto(format!(
            "OTP salt must be {OTP_SALT_LEN} bytes, got {}",
            salt.len()
        )));
    }
    let mut mac =
        HmacSha256::new_from_slice(pepper).map_err(|e| AuthError::Crypto(e.to_string()))?;
    mac.update(OTP_HASH_DOMAIN);
    mac.update(salt);
    mac.update(code.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&result);
    let mut salt_arr = [0u8; OTP_SALT_LEN];
    salt_arr.copy_from_slice(salt);
    Ok(OtpHash {
        digest,
        salt: salt_arr,
        keyed: true,
    })
}

/// Domain-separated SHA-256 without pepper/salt.
///
/// **Not offline-hard** — use only for tests or non-secret integrity demos.
/// Prefer [`hash_otp`] for anything persisted.
pub fn hash_otp_unkeyed(code: &str) -> Result<OtpHash> {
    validate_otp_format(code)?;
    let mut hasher = Sha256::new();
    hasher.update(OTP_HASH_DOMAIN);
    hasher.update(code.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(OtpHash {
        digest,
        salt: [0u8; OTP_SALT_LEN],
        keyed: false,
    })
}

/// Verify a plaintext code against a stored hash (constant-time digest compare).
///
/// For keyed hashes, pass the same `pepper` used at mint. For unkeyed hashes,
/// pass `&[]`.
pub fn verify_otp(code: &str, pepper: &[u8], expected: &OtpHash) -> Result<()> {
    let candidate = if expected.keyed {
        hash_otp_with_salt(code, pepper, &expected.salt)?
    } else {
        hash_otp_unkeyed(code)?
    };
    if bool::from(candidate.digest.ct_eq(&expected.digest)) {
        Ok(())
    } else {
        Err(AuthError::OtpVerify("code mismatch".into()))
    }
}

/// Convenience: generate OTP + keyed hash pair for minting.
pub fn mint_otp(digits: usize, pepper: &[u8]) -> Result<(OtpCode, OtpHash)> {
    let code = generate_otp(digits)?;
    let hash = hash_otp(code.as_str(), pepper)?;
    Ok((code, hash))
}

/// Mint with explicit expiry unix timestamp (keyed hash).
pub fn mint_otp_record(
    digits: usize,
    pepper: &[u8],
    expires_at_unix: u64,
) -> Result<(OtpCode, OtpRecord)> {
    let (code, hash) = mint_otp(digits, pepper)?;
    Ok((code, OtpRecord::new(hash, expires_at_unix)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    const PEPPER: &[u8] = b"test-otp-pepper-material!!";

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
    fn hash_and_verify_roundtrip_keyed() {
        let code = generate_otp(8).unwrap();
        let hash = hash_otp(code.as_str(), PEPPER).unwrap();
        assert!(hash.keyed);
        assert_ne!(hash.salt, [0u8; OTP_SALT_LEN]); // overwhelmingly likely
        assert!(verify_otp(code.as_str(), PEPPER, &hash).is_ok());
        assert!(verify_otp("00000000", PEPPER, &hash).is_err());
        assert!(verify_otp(code.as_str(), b"wrong-pepper!!!!!!!!!!", &hash).is_err());
    }

    #[test]
    fn hash_rejects_empty_pepper() {
        assert!(hash_otp("123456", b"").is_err());
    }

    #[test]
    fn unkeyed_hash_roundtrip() {
        let h1 = hash_otp_unkeyed("123456").unwrap();
        let h2 = hash_otp_unkeyed("123456").unwrap();
        assert!(!h1.keyed);
        assert!(h1.ct_eq(&h2));
        assert!(!h1.ct_eq(&hash_otp_unkeyed("123457").unwrap()));
        verify_otp("123456", &[], &h1).unwrap();
        assert!(verify_otp("123457", &[], &h1).is_err());
    }

    #[test]
    fn salt_changes_digest() {
        let s1 = [1u8; OTP_SALT_LEN];
        let s2 = [2u8; OTP_SALT_LEN];
        let h1 = hash_otp_with_salt("123456", PEPPER, &s1).unwrap();
        let h2 = hash_otp_with_salt("123456", PEPPER, &s2).unwrap();
        assert!(!bool::from(h1.digest.ct_eq(&h2.digest)));
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
        let (code, mut rec) = mint_otp_record(6, PEPPER, 1_700_000_100).unwrap();
        assert!(!rec.is_consumed());
        assert!(!rec.is_expired(1_700_000_000));
        assert!(rec.is_expired(1_700_000_100));

        // Expired
        assert!(matches!(
            rec.verify(code.as_str(), PEPPER, 1_700_000_100),
            Err(AuthError::OtpVerify(_))
        ));

        // Fresh verify
        rec.verify(code.as_str(), PEPPER, 1_700_000_000).unwrap();
        rec.verify_and_consume(code.as_str(), PEPPER, 1_700_000_000)
            .unwrap();
        assert!(rec.is_consumed());

        // Second consume fails
        assert!(matches!(
            rec.verify_and_consume(code.as_str(), PEPPER, 1_700_000_000),
            Err(AuthError::OtpVerify(_))
        ));
    }

    #[test]
    fn record_wrong_code() {
        let (_code, rec) = mint_otp_record(6, PEPPER, u64::MAX).unwrap();
        assert!(rec.verify("000000", PEPPER, 0).is_err());
    }

    #[test]
    fn mark_consumed_blocks_verify() {
        let (code, mut rec) = mint_otp_record(6, PEPPER, u64::MAX).unwrap();
        rec.mark_consumed();
        assert!(rec.verify(code.as_str(), PEPPER, 0).is_err());
    }

    #[test]
    fn mint_otp_pair() {
        let (code, hash) = mint_otp(7, PEPPER).unwrap();
        assert_eq!(code.len(), 7);
        verify_otp(code.as_str(), PEPPER, &hash).unwrap();
    }

    #[test]
    fn otp_debug_redacts_and_ui_string() {
        let code = generate_otp_default();
        let s = format!("{code:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains(code.as_str()));
        assert_eq!(code.to_ui_string(), code.as_str());
    }

    #[test]
    fn otp_hash_debug_and_ct_eq() {
        let h = hash_otp("123456", PEPPER).unwrap();
        let s = format!("{h:?}");
        assert!(s.contains("OtpHash"));
        assert!(h.ct_eq(&h.clone()));
        let other = hash_otp("654321", PEPPER).unwrap();
        assert!(!h.ct_eq(&other));
    }

    #[test]
    fn record_accessors() {
        let hash = hash_otp("654321", PEPPER).unwrap();
        let rec = OtpRecord::new(hash.clone(), 99);
        assert!(rec.hash().ct_eq(&hash));
        assert_eq!(rec.expires_at_unix(), 99);
    }

    #[test]
    fn bad_salt_length_rejected() {
        assert!(hash_otp_with_salt("123456", PEPPER, &[0u8; 8]).is_err());
    }
}
