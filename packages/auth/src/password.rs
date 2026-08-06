//! Mode C: Argon2id password hash + verify (server-side pre-filter only).

use crate::error::{AuthError, Result};
use argon2::{
    password_hash::{Ident, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Version,
};
use rand::rngs::OsRng;

/// Argon2id PHC algorithm identifier.
const ARGON2ID_IDENT: Ident<'_> = argon2::ARGON2ID_IDENT;

/// Build an Argon2id context (explicit algorithm pin for KD8).
fn argon2id() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Argon2::default().params().clone(),
    )
}

/// Hash a password with Argon2id (PHC string format).
///
/// Always uses **Argon2id** (not argon2i/argon2d). Suitable for Mode C server storage.
pub fn hash_password(password: &[u8]) -> Result<String> {
    if password.is_empty() {
        return Err(AuthError::Password("password must not be empty".into()));
    }
    let salt = SaltString::generate(&mut OsRng);
    argon2id()
        .hash_password(password, &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Password(e.to_string()))
}

/// Verify a password against a PHC-encoded **Argon2id** hash.
///
/// Rejects PHC strings that encode argon2i, argon2d, or other algorithms.
pub fn verify_password(password: &[u8], password_hash: &str) -> Result<()> {
    let parsed = PasswordHash::new(password_hash)
        .map_err(|e| AuthError::Password(format!("invalid hash: {e}")))?;
    if parsed.algorithm != ARGON2ID_IDENT {
        return Err(AuthError::Password(format!(
            "expected Argon2id, got algorithm '{}'",
            parsed.algorithm
        )));
    }
    argon2id()
        .verify_password(password, &parsed)
        .map_err(|_| AuthError::Password("password mismatch".into()))
}

/// Returns true if `password` matches an Argon2id `password_hash`.
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
        assert!(
            hash.starts_with("$argon2id$"),
            "expected Argon2id PHC prefix, got {hash}"
        );
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
    fn rejects_non_argon2id_algorithm() {
        // Valid PHC-shaped argon2i string (not necessarily a real hash of "x").
        // Algorithm pin must fail before password comparison.
        let argon2i = "$argon2i$v=19$m=16,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$ZGVhZGJlZWZkZWFkYmVlZg";
        let err = verify_password(b"x", argon2i).unwrap_err();
        match err {
            AuthError::Password(msg) => assert!(
                msg.contains("Argon2id") || msg.contains("invalid hash"),
                "unexpected message: {msg}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn different_hashes_same_password() {
        // Random salt → distinct encodings, both verify.
        let pw = b"same-password";
        let h1 = hash_password(pw).unwrap();
        let h2 = hash_password(pw).unwrap();
        assert_ne!(h1, h2);
        assert!(h1.starts_with("$argon2id$"));
        assert!(h2.starts_with("$argon2id$"));
        verify_password(pw, &h1).unwrap();
        verify_password(pw, &h2).unwrap();
    }
}
