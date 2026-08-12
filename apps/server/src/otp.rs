//! Mode A OTP hash store (in-memory; Postgres table in migration).
//!
//! Host mints an OTP locally, posts only the **hash** (digest + salt) with a TTL.
//! On `session_intent` with mode `otp`, the server prefilters the plaintext code
//! from `prefilter.otp` (rate-limited elsewhere), binds the row to the session
//! intent, and fully consumes on host accept.
//!
//! Pepper never leaves host/server config; hashes are keyed HMAC-SHA256 per
//! `remotelink_auth::hash_otp`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use remotelink_auth::{verify_otp, OtpHash, OTP_SALT_LEN};
use serde::{Deserialize, Serialize};

/// Default OTP lifetime when the host omits `expires_in_secs`.
pub const DEFAULT_OTP_TTL_SECS: u64 = 900;
/// Maximum accepted TTL (1 hour).
pub const MAX_OTP_TTL_SECS: u64 = 3600;
/// Cap failed verify attempts per stored OTP.
pub const MAX_OTP_ATTEMPTS: u32 = 5;

/// Default pepper for single-node / test deploys (override via env later).
pub const DEFAULT_OTP_PEPPER: &[u8] = b"remotelink-otp-server-pepper-v1!";

/// One stored OTP row (hash only; plaintext never persisted).
#[derive(Debug, Clone)]
pub struct StoredOtp {
    pub id: i64,
    pub host_device_id: i64,
    pub hash: OtpHash,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    /// Bound pending session when prefilter succeeds.
    pub session_intent_id: Option<String>,
    pub attempts: u32,
    pub created_at: DateTime<Utc>,
}

/// Result of a prefilter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtpPrefilterResult {
    /// Code matches; row bound to session intent (not yet consumed).
    Ok,
    /// No live OTP for this host.
    NoActiveOtp,
    /// Code mismatch / expired / already consumed / attempts exceeded.
    Reject,
}

/// Errors from the OTP store.
#[derive(Debug, thiserror::Error)]
pub enum OtpStoreError {
    #[error("no active OTP for host")]
    NotFound,
    #[error("OTP already consumed or expired")]
    Unavailable,
    #[error("OTP verification failed")]
    VerifyFailed,
    #[error("invalid OTP hash material: {0}")]
    BadHash(String),
}

/// In-memory OTP store (single-node).
#[derive(Debug, Default)]
pub struct MemoryOtpStore {
    inner: Mutex<OtpInner>,
    next_id: AtomicI64,
}

#[derive(Debug, Default)]
struct OtpInner {
    /// id → row
    by_id: HashMap<i64, StoredOtp>,
    /// host_device_id → active (unconsumed) otp ids (newest last)
    by_host: HashMap<i64, Vec<i64>>,
}

impl MemoryOtpStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a host-minted OTP hash with absolute expiry.
    pub fn store_hash(
        &self,
        host_device_id: i64,
        hash: OtpHash,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> StoredOtp {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let row = StoredOtp {
            id,
            host_device_id,
            hash,
            expires_at,
            consumed_at: None,
            session_intent_id: None,
            attempts: 0,
            created_at: now,
        };
        let mut g = self.inner.lock().expect("otp store lock");
        // Replace prior unconsumed OTPs for this host (single active window).
        let old_ids = g.by_host.remove(&host_device_id).unwrap_or_default();
        for old_id in old_ids {
            g.by_id.remove(&old_id);
        }
        g.by_host.insert(host_device_id, vec![row.id]);
        g.by_id.insert(row.id, row.clone());
        row
    }

    /// Host posts digest/salt hex; builds [`OtpHash`] and stores.
    pub fn store_from_parts(
        &self,
        host_device_id: i64,
        digest: [u8; 32],
        salt: [u8; OTP_SALT_LEN],
        keyed: bool,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> StoredOtp {
        self.store_hash(
            host_device_id,
            OtpHash {
                digest,
                salt,
                keyed,
            },
            expires_at,
            now,
        )
    }

    /// Convenience for tests: mint via auth helpers and store.
    pub fn mint_for_tests(
        &self,
        host_device_id: i64,
        code: &str,
        pepper: &[u8],
        ttl_secs: u64,
        now: DateTime<Utc>,
    ) -> Result<StoredOtp, OtpStoreError> {
        let hash = remotelink_auth::hash_otp(code, pepper)
            .map_err(|e| OtpStoreError::BadHash(e.to_string()))?;
        let expires_at = now + Duration::seconds(ttl_secs as i64);
        Ok(self.store_hash(host_device_id, hash, expires_at, now))
    }

    /// Prefilter: verify plaintext, bind to `session_id` without consuming.
    pub fn prefilter_bind(
        &self,
        host_device_id: i64,
        code: &str,
        pepper: &[u8],
        session_id: &str,
        now: DateTime<Utc>,
    ) -> OtpPrefilterResult {
        let mut g = self.inner.lock().expect("otp store lock");
        let Some(ids) = g.by_host.get(&host_device_id).cloned() else {
            return OtpPrefilterResult::NoActiveOtp;
        };
        // Prefer newest active row.
        for id in ids.into_iter().rev() {
            let Some(row) = g.by_id.get_mut(&id) else {
                continue;
            };
            if row.consumed_at.is_some() {
                continue;
            }
            if row.expires_at <= now {
                continue;
            }
            if row.attempts >= MAX_OTP_ATTEMPTS {
                return OtpPrefilterResult::Reject;
            }
            // A prior viewer attempt may have bound this row to a session that
            // never got accepted (app restart, timeout). Re-verify and rebind.
            row.attempts = row.attempts.saturating_add(1);
            let now_unix = now.timestamp().max(0) as u64;
            if row.expires_at.timestamp() <= now.timestamp() {
                return OtpPrefilterResult::Reject;
            }
            match verify_otp(code, pepper, &row.hash) {
                Ok(()) => {
                    // expiry double-check via unix for auth helper consistency
                    let _ = now_unix;
                    row.session_intent_id = Some(session_id.to_string());
                    // Successful verify does not consume; reset attempts noise.
                    row.attempts = row.attempts.saturating_sub(1);
                    return OtpPrefilterResult::Ok;
                }
                Err(_) => {
                    return OtpPrefilterResult::Reject;
                }
            }
        }
        OtpPrefilterResult::NoActiveOtp
    }

    /// Consume the OTP bound to `session_id` (host accept path).
    pub fn consume_for_session(
        &self,
        host_device_id: i64,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), OtpStoreError> {
        let mut g = self.inner.lock().expect("otp store lock");
        let ids = g.by_host.get(&host_device_id).cloned().unwrap_or_default();
        for id in ids {
            let Some(row) = g.by_id.get_mut(&id) else {
                continue;
            };
            if row.session_intent_id.as_deref() == Some(session_id) {
                if row.consumed_at.is_some() {
                    return Err(OtpStoreError::Unavailable);
                }
                if row.expires_at <= now {
                    return Err(OtpStoreError::Unavailable);
                }
                row.consumed_at = Some(now);
                return Ok(());
            }
        }
        Err(OtpStoreError::NotFound)
    }

    /// Lookup active (unconsumed, unexpired) OTP for a host, if any.
    pub fn active_for_host(&self, host_device_id: i64, now: DateTime<Utc>) -> Option<StoredOtp> {
        let g = self.inner.lock().expect("otp store lock");
        let ids = g.by_host.get(&host_device_id)?;
        for id in ids.iter().rev() {
            if let Some(row) = g.by_id.get(id) {
                if row.consumed_at.is_none() && row.expires_at > now {
                    return Some(row.clone());
                }
            }
        }
        None
    }

    /// Test helper: row by id.
    pub fn get(&self, id: i64) -> Option<StoredOtp> {
        self.inner
            .lock()
            .expect("otp store lock")
            .by_id
            .get(&id)
            .cloned()
    }
}

/// Public response after a successful mint store.
#[derive(Debug, Clone, Serialize)]
pub struct OtpMintResponse {
    pub expires_at: String,
    /// Opaque server row id (not secret).
    pub otp_id: i64,
}

/// Host mint request body.
#[derive(Debug, Clone, Deserialize)]
pub struct OtpMintRequest {
    /// Hex-encoded 32-byte digest.
    pub digest_hex: String,
    /// Hex-encoded 16-byte salt.
    pub salt_hex: String,
    /// Whether the digest is HMAC-keyed (default true).
    #[serde(default = "default_true")]
    pub keyed: bool,
    /// Lifetime in seconds (default 300, max 3600).
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl OtpMintRequest {
    /// Parse digest/salt into raw bytes.
    pub fn parse_hash(&self) -> Result<OtpHash, OtpStoreError> {
        let digest = parse_hex_exact::<32>(&self.digest_hex)
            .map_err(|e| OtpStoreError::BadHash(format!("digest: {e}")))?;
        let salt = parse_hex_exact::<OTP_SALT_LEN>(&self.salt_hex)
            .map_err(|e| OtpStoreError::BadHash(format!("salt: {e}")))?;
        Ok(OtpHash {
            digest,
            salt,
            keyed: self.keyed,
        })
    }

    pub fn ttl_secs(&self) -> Result<u64, OtpStoreError> {
        let secs = self.expires_in_secs.unwrap_or(DEFAULT_OTP_TTL_SECS);
        if secs == 0 || secs > MAX_OTP_TTL_SECS {
            return Err(OtpStoreError::BadHash(format!(
                "expires_in_secs must be 1..={MAX_OTP_TTL_SECS}"
            )));
        }
        Ok(secs)
    }
}

fn parse_hex_exact<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let bytes = hex::decode(s.trim()).map_err(|e| e.to_string())?;
    if bytes.len() != N {
        return Err(format!("expected {N} bytes, got {}", bytes.len()));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_auth::mint_otp;

    const PEPPER: &[u8] = DEFAULT_OTP_PEPPER;

    #[test]
    fn mint_prefilter_consume_once() {
        let store = MemoryOtpStore::new();
        let now = Utc::now();
        let (code, hash) = mint_otp(6, PEPPER).unwrap();
        let row = store.store_hash(1, hash, now + Duration::seconds(300), now);
        assert!(store.active_for_host(1, now).is_some());

        assert_eq!(
            store.prefilter_bind(1, code.as_str(), PEPPER, "sess-1", now),
            OtpPrefilterResult::Ok
        );
        // Wrong code rejects.
        let (code2, hash2) = mint_otp(6, PEPPER).unwrap();
        let _ = code2;
        store.store_hash(2, hash2, now + Duration::seconds(300), now);
        assert_eq!(
            store.prefilter_bind(2, "000000", PEPPER, "sess-x", now),
            OtpPrefilterResult::Reject
        );

        store.consume_for_session(1, "sess-1", now).unwrap();
        let got = store.get(row.id).unwrap();
        assert!(got.consumed_at.is_some());
        // Second consume fails.
        assert!(store.consume_for_session(1, "sess-1", now).is_err());
        // Prefilter after consume: no active.
        assert_eq!(
            store.prefilter_bind(1, code.as_str(), PEPPER, "sess-2", now),
            OtpPrefilterResult::NoActiveOtp
        );
    }

    #[test]
    fn prefilter_rebinds_when_first_session_never_accepted() {
        let store = MemoryOtpStore::new();
        let now = Utc::now();
        let (code, hash) = mint_otp(6, PEPPER).unwrap();
        store.store_hash(3, hash, now + Duration::seconds(300), now);
        assert_eq!(
            store.prefilter_bind(3, code.as_str(), PEPPER, "viewer-1", now),
            OtpPrefilterResult::Ok
        );
        assert_eq!(
            store.prefilter_bind(3, code.as_str(), PEPPER, "viewer-2", now),
            OtpPrefilterResult::Ok
        );
        store.consume_for_session(3, "viewer-2", now).unwrap();
        assert!(store.consume_for_session(3, "viewer-1", now).is_err());
    }

    #[test]
    fn replace_active_window() {
        let store = MemoryOtpStore::new();
        let now = Utc::now();
        let (c1, h1) = mint_otp(6, PEPPER).unwrap();
        store.store_hash(9, h1, now + Duration::seconds(300), now);
        let (c2, h2) = mint_otp(6, PEPPER).unwrap();
        store.store_hash(9, h2, now + Duration::seconds(300), now);
        // Old code mismatches the replacement window (still one active row).
        assert_eq!(
            store.prefilter_bind(9, c1.as_str(), PEPPER, "s", now),
            OtpPrefilterResult::Reject
        );
        assert_eq!(
            store.prefilter_bind(9, c2.as_str(), PEPPER, "s2", now),
            OtpPrefilterResult::Ok
        );
    }
}
