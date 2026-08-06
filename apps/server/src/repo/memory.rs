//! In-memory repository for unit tests and local runs without Postgres.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{DeviceRepository, RepoError};
use crate::models::{Device, DeviceCredential, DeviceStatus, NewCredential, NewDevice};

/// Thread-safe in-memory device registry.
#[derive(Debug, Default)]
pub struct MemoryDeviceRepo {
    next_device_id: AtomicI64,
    next_cred_id: AtomicI64,
    inner: Mutex<Store>,
}

#[derive(Debug, Default)]
struct Store {
    devices: HashMap<i64, Device>,
    by_public_id: HashMap<String, i64>,
    credentials: HashMap<i64, DeviceCredential>,
}

impl MemoryDeviceRepo {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>, RepoError> {
        self.inner
            .lock()
            .map_err(|_| RepoError::Internal("memory store poisoned".into()))
    }
}

#[async_trait]
impl DeviceRepository for MemoryDeviceRepo {
    async fn create_device(&self, new: NewDevice) -> Result<Device, RepoError> {
        let mut store = self.lock()?;
        if store.by_public_id.contains_key(&new.public_id) {
            return Err(RepoError::Conflict(format!(
                "public_id {} already exists",
                new.public_id
            )));
        }
        let id = self.next_device_id.fetch_add(1, Ordering::SeqCst) + 1;
        let device = Device {
            id,
            public_id: new.public_id.clone(),
            display_name: new.display_name,
            public_key: new.public_key,
            password_hash: None,
            protocol_version_last: new.protocol_version_last,
            created_at: Utc::now(),
            last_seen_at: None,
            status: DeviceStatus::Active,
            deleted_at: None,
        };
        store.by_public_id.insert(new.public_id, id);
        store.devices.insert(id, device.clone());
        Ok(device)
    }

    async fn get_by_public_id(&self, public_id: &str) -> Result<Option<Device>, RepoError> {
        let store = self.lock()?;
        Ok(store
            .by_public_id
            .get(public_id)
            .and_then(|id| store.devices.get(id).cloned()))
    }

    async fn get_by_id(&self, id: i64) -> Result<Option<Device>, RepoError> {
        let store = self.lock()?;
        Ok(store.devices.get(&id).cloned())
    }

    async fn soft_delete(&self, public_id: &str, at: DateTime<Utc>) -> Result<bool, RepoError> {
        let mut store = self.lock()?;
        let Some(&id) = store.by_public_id.get(public_id) else {
            return Ok(false);
        };
        let Some(device) = store.devices.get_mut(&id) else {
            return Ok(false);
        };
        if device.status == DeviceStatus::Deleted {
            return Ok(true);
        }
        device.status = DeviceStatus::Deleted;
        device.deleted_at = Some(at);
        Ok(true)
    }

    async fn insert_credential(&self, new: NewCredential) -> Result<DeviceCredential, RepoError> {
        let mut store = self.lock()?;
        if !store.devices.contains_key(&new.device_id) {
            return Err(RepoError::NotFound);
        }
        let id = self.next_cred_id.fetch_add(1, Ordering::SeqCst) + 1;
        let cred = DeviceCredential {
            id,
            device_id: new.device_id,
            token_hash: new.token_hash,
            refresh_token_hash: new.refresh_token_hash,
            expires_at: new.expires_at,
            revoked_at: None,
            created_at: Utc::now(),
        };
        store.credentials.insert(id, cred.clone());
        Ok(cred)
    }

    async fn find_by_access_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError> {
        let store = self.lock()?;
        let cred = store.credentials.values().find(|c| {
            c.revoked_at.is_none() && crate::credentials::token_hash_eq(&c.token_hash, token_hash)
        });
        match cred {
            Some(c) => {
                let device = store
                    .devices
                    .get(&c.device_id)
                    .cloned()
                    .ok_or_else(|| RepoError::Internal("credential orphan".into()))?;
                Ok(Some((device, c.clone())))
            }
            None => Ok(None),
        }
    }

    async fn find_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError> {
        let store = self.lock()?;
        let cred = store.credentials.values().find(|c| {
            c.revoked_at.is_none()
                && crate::credentials::token_hash_eq(&c.refresh_token_hash, refresh_token_hash)
        });
        match cred {
            Some(c) => {
                let device = store
                    .devices
                    .get(&c.device_id)
                    .cloned()
                    .ok_or_else(|| RepoError::Internal("credential orphan".into()))?;
                Ok(Some((device, c.clone())))
            }
            None => Ok(None),
        }
    }

    async fn revoke_credential(
        &self,
        credential_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut store = self.lock()?;
        let Some(cred) = store.credentials.get_mut(&credential_id) else {
            return Err(RepoError::NotFound);
        };
        cred.revoked_at = Some(at);
        Ok(())
    }

    async fn revoke_all_for_device(
        &self,
        device_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut store = self.lock()?;
        for cred in store.credentials.values_mut() {
            if cred.device_id == device_id && cred.revoked_at.is_none() {
                cred.revoked_at = Some(at);
            }
        }
        Ok(())
    }

    async fn touch_last_seen(&self, device_id: i64, at: DateTime<Utc>) -> Result<(), RepoError> {
        let mut store = self.lock()?;
        let Some(device) = store.devices.get_mut(&device_id) else {
            return Err(RepoError::NotFound);
        };
        device.last_seen_at = Some(at);
        Ok(())
    }

    async fn ping(&self) -> Result<(), RepoError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_lookup() {
        let repo = MemoryDeviceRepo::new();
        let d = repo
            .create_device(NewDevice {
                public_id: "1234567897".into(),
                display_name: Some("desk".into()),
                public_key: vec![1; 32],
                protocol_version_last: Some(1),
            })
            .await
            .unwrap();
        assert_eq!(d.public_id, "1234567897");
        let found = repo.get_by_public_id("1234567897").await.unwrap().unwrap();
        assert_eq!(found.id, d.id);
    }

    #[tokio::test]
    async fn soft_delete_and_revoke() {
        let repo = MemoryDeviceRepo::new();
        let d = repo
            .create_device(NewDevice {
                public_id: "1234567897".into(),
                display_name: None,
                public_key: vec![2; 32],
                protocol_version_last: None,
            })
            .await
            .unwrap();
        let now = Utc::now();
        let cred = repo
            .insert_credential(NewCredential {
                device_id: d.id,
                token_hash: "aa".into(),
                refresh_token_hash: "bb".into(),
                expires_at: now + chrono::Duration::hours(1),
            })
            .await
            .unwrap();
        repo.revoke_credential(cred.id, now).await.unwrap();
        assert!(repo.find_by_access_hash("aa").await.unwrap().is_none());
        assert!(repo.soft_delete("1234567897", now).await.unwrap());
        let d2 = repo.get_by_public_id("1234567897").await.unwrap().unwrap();
        assert_eq!(d2.status, DeviceStatus::Deleted);
    }
}
