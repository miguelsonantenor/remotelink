//! Numeric public device IDs with Luhn check digits (KD14).
//!
//! Public IDs are human-friendly, non-secret identifiers used for call-out UX
//! (e.g. “connect to host `123 456 7890`”). They are **not** credentials.
//!
//! Format:
//! - 9 body digits (random)
//! - 1 Luhn check digit
//! - total 10 digits; optional display groups of 3-3-4

use crate::error::{AuthError, Result};
use rand::rngs::OsRng;
use rand::Rng;

/// Number of random body digits before the check digit.
pub const DEVICE_ID_BODY_DIGITS: usize = 9;

/// Total digit length including the Luhn check digit.
pub const DEVICE_ID_TOTAL_DIGITS: usize = DEVICE_ID_BODY_DIGITS + 1;

/// A validated numeric public device ID (KD14).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DevicePublicId {
    /// Canonical digit string, length [`DEVICE_ID_TOTAL_DIGITS`], no separators.
    digits: String,
}

impl DevicePublicId {
    /// Generate a new random public device ID with a valid Luhn check digit.
    pub fn generate() -> Self {
        Self::generate_with_rng(&mut OsRng)
    }

    /// Generate using a provided RNG (testable).
    pub fn generate_with_rng<R: Rng + ?Sized>(rng: &mut R) -> Self {
        let mut body = String::with_capacity(DEVICE_ID_BODY_DIGITS);
        for _ in 0..DEVICE_ID_BODY_DIGITS {
            // First digit non-zero so IDs don't look like short numbers.
            if body.is_empty() {
                body.push(char::from(b'1' + rng.gen_range(0..9)));
            } else {
                body.push(char::from(b'0' + rng.gen_range(0..10)));
            }
        }
        // Body is always ASCII digits by construction.
        let check = luhn_check_digit(&body).expect("body digits are numeric");
        let mut digits = body;
        digits.push(char::from(b'0' + check));
        Self { digits }
    }

    /// Parse and validate a public ID string (digits only, or with spaces/dashes).
    pub fn parse(input: &str) -> Result<Self> {
        let digits: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
            .collect();

        if digits.len() != DEVICE_ID_TOTAL_DIGITS {
            return Err(AuthError::InvalidDeviceId(format!(
                "expected {DEVICE_ID_TOTAL_DIGITS} digits, got {}",
                digits.len()
            )));
        }
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(AuthError::InvalidDeviceId(
                "public ID must be numeric".into(),
            ));
        }
        if !validate_luhn(&digits) {
            return Err(AuthError::InvalidDeviceId("check digit mismatch".into()));
        }
        Ok(Self { digits })
    }

    /// Return true if `input` is a valid public ID (convenience wrapper).
    pub fn is_valid(input: &str) -> bool {
        Self::parse(input).is_ok()
    }

    /// Canonical digit string (no separators).
    pub fn as_str(&self) -> &str {
        &self.digits
    }

    /// Digits as owned string.
    pub fn into_string(self) -> String {
        self.digits
    }

    /// Human-friendly display form: groups of 3-3-4 (e.g. `123 456 7890`).
    pub fn display_grouped(&self) -> String {
        let d = &self.digits;
        format!("{} {} {}", &d[0..3], &d[3..6], &d[6..10])
    }
}

impl std::fmt::Display for DevicePublicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_grouped())
    }
}

impl std::str::FromStr for DevicePublicId {
    type Err = AuthError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Compute Luhn check digit for a string of body digits (no check digit yet).
///
/// Returns 0–9. Errors if `body_digits` is empty or contains non-ASCII digits.
pub fn luhn_check_digit(body_digits: &str) -> Result<u8> {
    if body_digits.is_empty() {
        return Err(AuthError::InvalidDeviceId(
            "Luhn body must be non-empty".into(),
        ));
    }
    if !body_digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(AuthError::InvalidDeviceId(
            "Luhn body must be numeric digits only".into(),
        ));
    }
    // Luhn over body + '0', then check digit is (10 - (sum % 10)) % 10.
    let mut sum: u32 = 0;
    let mut double = true; // position from right: first (check) is not doubled; body last is doubled
    for c in body_digits.chars().rev() {
        let mut d = c.to_digit(10).expect("checked ascii digit");
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    Ok(((10 - (sum % 10)) % 10) as u8)
}

/// Validate full digit string including check digit via Luhn.
pub fn validate_luhn(full_digits: &str) -> bool {
    if full_digits.is_empty() || !full_digits.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut sum: u32 = 0;
    let mut double = false;
    for c in full_digits.chars().rev() {
        let mut d = c.to_digit(10).unwrap();
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn generate_has_correct_length_and_valid_luhn() {
        let id = DevicePublicId::generate();
        assert_eq!(id.as_str().len(), DEVICE_ID_TOTAL_DIGITS);
        assert!(validate_luhn(id.as_str()));
        assert!(id.as_str().chars().all(|c| c.is_ascii_digit()));
        // Leading digit non-zero
        assert_ne!(id.as_str().as_bytes()[0], b'0');
    }

    #[test]
    fn generate_with_rng_is_deterministic() {
        let mut a = StdRng::seed_from_u64(42);
        let mut b = StdRng::seed_from_u64(42);
        let id_a = DevicePublicId::generate_with_rng(&mut a);
        let id_b = DevicePublicId::generate_with_rng(&mut b);
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn parse_accepts_grouped_and_plain() {
        let id = DevicePublicId::generate();
        let plain = id.as_str().to_string();
        let grouped = id.display_grouped();
        assert_eq!(DevicePublicId::parse(&plain).unwrap().as_str(), plain);
        assert_eq!(DevicePublicId::parse(&grouped).unwrap().as_str(), plain);
        // dashes / underscores as separators
        let dashed = format!("{}-{}-{}", &plain[0..3], &plain[3..6], &plain[6..10]);
        assert_eq!(DevicePublicId::parse(&dashed).unwrap().as_str(), plain);
        let underscored = format!("{}_{}_{}", &plain[0..3], &plain[3..6], &plain[6..10]);
        assert_eq!(DevicePublicId::parse(&underscored).unwrap().as_str(), plain);
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(matches!(
            DevicePublicId::parse("123"),
            Err(AuthError::InvalidDeviceId(_))
        ));
        assert!(matches!(
            DevicePublicId::parse("12345678901"),
            Err(AuthError::InvalidDeviceId(_))
        ));
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(matches!(
            DevicePublicId::parse("123456789a"),
            Err(AuthError::InvalidDeviceId(_))
        ));
    }

    #[test]
    fn parse_rejects_bad_check_digit() {
        let id = DevicePublicId::generate();
        let mut digits: Vec<u8> = id.as_str().bytes().collect();
        // Flip check digit
        let last = digits.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let bad = String::from_utf8(digits).unwrap();
        assert!(matches!(
            DevicePublicId::parse(&bad),
            Err(AuthError::InvalidDeviceId(_))
        ));
        assert!(!DevicePublicId::is_valid(&bad));
    }

    #[test]
    fn is_valid_true_for_generated() {
        let id = DevicePublicId::generate();
        assert!(DevicePublicId::is_valid(id.as_str()));
        assert!(DevicePublicId::is_valid(&id.display_grouped()));
    }

    #[test]
    fn display_grouped_format() {
        // Body 123456789 → check digit 7.
        let id = DevicePublicId::parse("1234567897").expect("known valid ID");
        assert_eq!(id.display_grouped(), "123 456 7897");
        assert_eq!(id.to_string(), "123 456 7897");
    }

    #[test]
    fn known_luhn_vector() {
        assert_eq!(luhn_check_digit("123456789").unwrap(), 7);
        assert!(validate_luhn("1234567897"));
        assert_eq!(luhn_check_digit("799273987").unwrap(), 5);
        assert!(validate_luhn("7992739875"));
        let id: DevicePublicId = "1234567897".parse().unwrap();
        assert_eq!(id.as_str(), "1234567897");
        assert_eq!(id.into_string(), "1234567897");
    }

    #[test]
    fn luhn_check_digit_rejects_non_numeric() {
        assert!(luhn_check_digit("").is_err());
        assert!(luhn_check_digit("12a").is_err());
        assert!(luhn_check_digit("12 3").is_err());
    }

    #[test]
    fn luhn_check_digit_zero_case() {
        // body "000000000" → sum path; check digit for all zeros body.
        let check = luhn_check_digit("000000000").unwrap();
        let mut full = String::from("000000000");
        full.push(char::from(b'0' + check));
        assert!(validate_luhn(&full));
        assert_eq!(full.len(), 10);
    }

    #[test]
    fn validate_luhn_rejects_empty_and_alpha() {
        assert!(!validate_luhn(""));
        assert!(!validate_luhn("abcdef"));
        assert!(!validate_luhn("12345x"));
    }

    #[test]
    fn many_generated_ids_are_unique_and_valid() {
        let mut set = std::collections::HashSet::new();
        for _ in 0..64 {
            let id = DevicePublicId::generate();
            assert!(validate_luhn(id.as_str()));
            set.insert(id.into_string());
        }
        // Extremely unlikely to collide 64 times for 9 random digits.
        assert!(set.len() > 1);
    }
}
