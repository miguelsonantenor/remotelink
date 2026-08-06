//! Opaque device credential minting and hashing.

use chrono::{DateTime, Duration, Utc};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::models::{IssuedTokens, NewCredential};

/// Access token lifetime (enforced via `access_expires_at`).
pub const ACCESS_TOKEN_TTL: Duration = Duration::hours(24);

/// Refresh token lifetime (enforced via credential `expires_at`).
pub const REFRESH_TOKEN_TTL: Duration = Duration::days(30);

const TOKEN_BYTES: usize = 32;

/// Mint a random access + refresh token pair with access expiry.
pub fn mint_tokens(now: DateTime<Utc>) -> IssuedTokens {
    IssuedTokens {
        access_token: random_token("rl_at_"),
        refresh_token: random_token("rl_rt_"),
        expires_at: access_expires_at(now),
    }
}

/// Build insertable credential hashes from issued tokens.
pub fn new_credential_from_issued(
    device_id: i64,
    issued: &IssuedTokens,
    now: DateTime<Utc>,
) -> NewCredential {
    NewCredential {
        device_id,
        token_hash: hash_token(&issued.access_token),
        refresh_token_hash: hash_token(&issued.refresh_token),
        access_expires_at: issued.expires_at,
        expires_at: refresh_expires_at(now),
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

/// Access token expiry timestamp.
pub fn access_expires_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + ACCESS_TOKEN_TTL
}

/// Refresh / credential-row expiry timestamp.
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
        let nc = new_credential_from_issued(1, &t, now);
        assert_eq!(nc.access_expires_at, t.expires_at);
        assert_eq!(nc.expires_at, now + REFRESH_TOKEN_TTL);
        assert_ne!(nc.access_expires_at, nc.expires_at);
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
