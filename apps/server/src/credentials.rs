//! Opaque device credential minting and hashing.

use chrono::{DateTime, Duration, Utc};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::models::IssuedTokens;

/// Access token lifetime.
pub const ACCESS_TOKEN_TTL: Duration = Duration::hours(24);

/// Refresh token lifetime (also used as credential `expires_at`).
pub const REFRESH_TOKEN_TTL: Duration = Duration::days(30);

const TOKEN_BYTES: usize = 32;

/// Mint a random access + refresh token pair with expiry.
pub fn mint_tokens(now: DateTime<Utc>) -> IssuedTokens {
    IssuedTokens {
        access_token: random_token("rl_at_"),
        refresh_token: random_token("rl_rt_"),
        expires_at: now + ACCESS_TOKEN_TTL,
    }
}

/// Hash a token for durable storage (SHA-256 hex).
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Constant-time-ish equality for hex digests (length-checked).
pub fn token_hash_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Credential row expiry uses the longer refresh window.
pub fn refresh_expires_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + REFRESH_TOKEN_TTL
}

fn random_token(prefix: &str) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
    bytes.zeroize();
    format!("{prefix}{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let h = hash_token("rl_at_test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, hash_token("rl_at_test"));
        assert_ne!(h, hash_token("rl_at_other"));
    }

    #[test]
    fn mint_tokens_have_prefixes() {
        let now = Utc::now();
        let t = mint_tokens(now);
        assert!(t.access_token.starts_with("rl_at_"));
        assert!(t.refresh_token.starts_with("rl_rt_"));
        assert_eq!(t.expires_at, now + ACCESS_TOKEN_TTL);
    }

    #[test]
    fn token_hash_eq_rejects_mismatch() {
        let a = hash_token("a");
        let b = hash_token("b");
        assert!(token_hash_eq(&a, &a));
        assert!(!token_hash_eq(&a, &b));
        assert!(!token_hash_eq(&a, "short"));
    }
}
