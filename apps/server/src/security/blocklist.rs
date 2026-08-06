//! Per-host blocklist of viewer fingerprints, IPs, and device IDs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What is being blocked for a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSubjectType {
    Ip,
    ViewerFingerprint,
    Device,
}

impl BlockSubjectType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::ViewerFingerprint => "viewer_fingerprint",
            Self::Device => "device",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ip" => Some(Self::Ip),
            "viewer_fingerprint" => Some(Self::ViewerFingerprint),
            "device" => Some(Self::Device),
            _ => None,
        }
    }
}

/// One blocklist entry (subject stored as hash only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlocklistEntry {
    pub id: i64,
    pub host_device_id: i64,
    pub subject_type: BlockSubjectType,
    pub subject_hash: String,
    pub created_at: DateTime<Utc>,
}

/// Insert request.
#[derive(Debug, Clone)]
pub struct NewBlocklistEntry {
    pub host_device_id: i64,
    pub subject_type: BlockSubjectType,
    /// Already-hashed subject (or use [`hash_subject`]).
    pub subject_hash: String,
}

/// Blocklist errors.
#[derive(Debug, thiserror::Error)]
pub enum BlocklistError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// SHA-256 hex of a subject string (IP, fingerprint, device public id).
pub fn hash_subject(subject: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(subject.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Async per-host blocklist.
#[async_trait]
pub trait BlocklistStore: Send + Sync {
    async fn add(&self, entry: NewBlocklistEntry) -> Result<BlocklistEntry, BlocklistError>;

    async fn list_for_host(
        &self,
        host_device_id: i64,
    ) -> Result<Vec<BlocklistEntry>, BlocklistError>;

    async fn is_blocked(
        &self,
        host_device_id: i64,
        subject_type: BlockSubjectType,
        subject_hash: &str,
    ) -> Result<bool, BlocklistError>;

    async fn remove(&self, host_device_id: i64, entry_id: i64) -> Result<(), BlocklistError>;
}

#[derive(Debug, Default)]
struct Store {
    entries: HashMap<i64, BlocklistEntry>,
    /// (host, type, hash) → entry id for uniqueness.
    index: HashMap<(i64, BlockSubjectType, String), i64>,
}

/// In-memory blocklist (tests / single-node).
#[derive(Debug, Default)]
pub struct MemoryBlocklist {
    next_id: AtomicI64,
    inner: Mutex<Store>,
}

impl MemoryBlocklist {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BlocklistStore for MemoryBlocklist {
    async fn add(&self, entry: NewBlocklistEntry) -> Result<BlocklistEntry, BlocklistError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| BlocklistError::Internal("blocklist poisoned".into()))?;

        let key = (
            entry.host_device_id,
            entry.subject_type,
            entry.subject_hash.clone(),
        );
        if g.index.contains_key(&key) {
            return Err(BlocklistError::Conflict(
                "subject already blocked for host".into(),
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let row = BlocklistEntry {
            id,
            host_device_id: entry.host_device_id,
            subject_type: entry.subject_type,
            subject_hash: entry.subject_hash,
            created_at: Utc::now(),
        };
        g.index.insert(key, id);
        g.entries.insert(id, row.clone());
        Ok(row)
    }

    async fn list_for_host(
        &self,
        host_device_id: i64,
    ) -> Result<Vec<BlocklistEntry>, BlocklistError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| BlocklistError::Internal("blocklist poisoned".into()))?;
        let mut list: Vec<_> = g
            .entries
            .values()
            .filter(|e| e.host_device_id == host_device_id)
            .cloned()
            .collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(list)
    }

    async fn is_blocked(
        &self,
        host_device_id: i64,
        subject_type: BlockSubjectType,
        subject_hash: &str,
    ) -> Result<bool, BlocklistError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| BlocklistError::Internal("blocklist poisoned".into()))?;
        Ok(g.index
            .contains_key(&(host_device_id, subject_type, subject_hash.to_string())))
    }

    async fn remove(&self, host_device_id: i64, entry_id: i64) -> Result<(), BlocklistError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| BlocklistError::Internal("blocklist poisoned".into()))?;
        let Some(entry) = g.entries.get(&entry_id).cloned() else {
            return Err(BlocklistError::NotFound);
        };
        if entry.host_device_id != host_device_id {
            return Err(BlocklistError::NotFound);
        }
        g.entries.remove(&entry_id);
        g.index
            .remove(&(entry.host_device_id, entry.subject_type, entry.subject_hash));
        Ok(())
    }
}

/// Check whether any of the provided subjects is blocked for the host.
pub async fn any_blocked(
    store: &dyn BlocklistStore,
    host_device_id: i64,
    subjects: &[(BlockSubjectType, String)],
) -> Result<Option<(BlockSubjectType, String)>, BlocklistError> {
    for (ty, hash) in subjects {
        if store.is_blocked(host_device_id, *ty, hash).await? {
            return Ok(Some((*ty, hash.clone())));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_list_check_remove() {
        let bl = MemoryBlocklist::new();
        let hash = hash_subject("203.0.113.9");
        let entry = bl
            .add(NewBlocklistEntry {
                host_device_id: 42,
                subject_type: BlockSubjectType::Ip,
                subject_hash: hash.clone(),
            })
            .await
            .unwrap();

        assert!(bl
            .is_blocked(42, BlockSubjectType::Ip, &hash)
            .await
            .unwrap());
        assert!(!bl
            .is_blocked(42, BlockSubjectType::Device, &hash)
            .await
            .unwrap());
        assert!(!bl
            .is_blocked(99, BlockSubjectType::Ip, &hash)
            .await
            .unwrap());

        let list = bl.list_for_host(42).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, entry.id);

        // Duplicate → conflict
        let err = bl
            .add(NewBlocklistEntry {
                host_device_id: 42,
                subject_type: BlockSubjectType::Ip,
                subject_hash: hash.clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, BlocklistError::Conflict(_)));

        bl.remove(42, entry.id).await.unwrap();
        assert!(!bl
            .is_blocked(42, BlockSubjectType::Ip, &hash)
            .await
            .unwrap());
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_subject("abc"), hash_subject("abc"));
        assert_ne!(hash_subject("abc"), hash_subject("abd"));
        assert_eq!(hash_subject("  x  "), hash_subject("x"));
    }
}
