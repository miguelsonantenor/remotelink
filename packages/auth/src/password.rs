//! Mode C: Argon2id password hash + verify (server-side pre-filter only).

use crate::error::{AuthError, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::rngs::OsRng;

/// Hash a password with Argon2id (PHC string format).
///
/// Uses the `argon2` crate defaults (Argon2id). Suitable for Mode C server storage.
pub fn hash_password(password: &[u8]) -> Result<String> {
    if password.is_empty() {
        return Err(AuthError::Password("password must not be empty".into()));
    }
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password, &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Password(e.to_string()))
}

/// Verify a password against a PHC-encoded Argon2id hash.
pub fn verify_password(password: &[u8], password_hash: &str) -> Result<()> {
    let parsed = PasswordHash::new(password_hash)
        .map_err(|e| AuthError::Password(format!("invalid hash: {e}")))?;
    Argon2::default()
        .verify_password(password, &parsed)
        .map_err(|_| AuthError::Password("password mismatch".into()))
}

/// Returns true if `password` matches `password_hash`.
pub fn password_matches(password: &[u8], password_hash: &str) -> bool {
    verify_password(password, password_hash).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_verify_roundtrip() {
        let pw = b"correct horse battery staple";
        let hash = hash_password(pw).unwrap();
        assert!(hash.starts_with("$argon2"));
        verify_password(pw, &hash).unwrap();
        assert!(password_matches(pw, &hash));
    }

    #[test]
    fn wrong_password_fails() {
        let hash = hash_password(b"secret-one").unwrap();
        assert!(verify_password(b"secret-two", &hash).is_err());
        assert!(!password_matches(b"secret-two", &hash));
    }

    #[test]
    fn empty_password_rejected() {
        assert!(matches!(hash_password(b""), Err(AuthError::Password(_))));
    }

    #[test]
    fn invalid_hash_string() {
        assert!(matches!(
            verify_password(b"x", "not-a-valid-phc"),
            Err(AuthError::Password(_))
        ));
    }

    #[test]
    fn different_hashes_same_password() {
        // Random salt → distinct encodings, both verify.
        let pw = b"same-password";
        let h1 = hash_password(pw).unwrap();
        let h2 = hash_password(pw).unwrap();
        assert_ne!(h1, h2);
        verify_password(pw, &h1).unwrap();
        verify_password(pw, &h2).unwrap();
    }
}
