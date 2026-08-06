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
    /// Create an empty in-memory repository.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: force a device status (e.g. disabled).
    pub fn set_status(&self, public_id: &str, status: DeviceStatus) -> Result<(), RepoError> {
        let mut store = self.lock()?;
        let Some(&id) = store.by_public_id.get(public_id) else {
            return Err(RepoError::NotFound);
        };
        let Some(device) = store.devices.get_mut(&id) else {
            return Err(RepoError::NotFound);
        };
        device.status = status;
        if status == DeviceStatus::Deleted {
            device.deleted_at = Some(Utc::now());
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>, RepoError> {
        self.inner
            .lock()
            .map_err(|_| RepoError::Internal("memory store poisoned".into()))
    }

    fn insert_credential_locked(
        store: &mut Store,
        next_cred_id: &AtomicI64,
        new: NewCredential,
        created_at: DateTime<Utc>,
    ) -> Result<DeviceCredential, RepoError> {
        if !store.devices.contains_key(&new.device_id) {
            return Err(RepoError::NotFound);
        }
        let id = next_cred_id.fetch_add(1, Ordering::SeqCst) + 1;
        let cred = DeviceCredential {
            id,
            device_id: new.device_id,
            token_hash: new.token_hash,
            refresh_token_hash: new.refresh_token_hash,
            access_expires_at: new.access_expires_at,
            expires_at: new.expires_at,
            revoked_at: None,
            created_at,
        };
        store.credentials.insert(id, cred.clone());
        Ok(cred)
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
        Self::insert_credential_locked(&mut store, &self.next_cred_id, new, Utc::now())
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

    async fn rotate_refresh(
        &self,
        refresh_token_hash: &str,
        new: NewCredential,
        now: DateTime<Utc>,
    ) -> Result<(Device, DeviceCredential), RepoError> {
        let mut store = self.lock()?;
        let old_id = store
            .credentials
            .values()
            .find(|c| {
                c.revoked_at.is_none()
                    && crate::credentials::token_hash_eq(&c.refresh_token_hash, refresh_token_hash)
            })
            .map(|c| c.id);

        let Some(old_id) = old_id else {
            return Err(RepoError::StaleCredential);
        };

        let (device_id, expires_at) = {
            let old = store
                .credentials
                .get(&old_id)
                .ok_or(RepoError::StaleCredential)?;
            if old.expires_at < now {
                return Err(RepoError::StaleCredential);
            }
            (old.device_id, old.expires_at)
        };
        // silence unused if we only need device_id — expires_at already checked
        let _ = expires_at;

        let device = store
            .devices
            .get(&device_id)
            .cloned()
            .ok_or_else(|| RepoError::Internal("credential orphan".into()))?;
        if device.status != DeviceStatus::Active {
            return Err(RepoError::StaleCredential);
        }

        // Bind new credential to the same device as the refresh token.
        if new.device_id != device_id {
            return Err(RepoError::Internal(
                "rotate_refresh device_id mismatch".into(),
            ));
        }

        let old = store
            .credentials
            .get_mut(&old_id)
            .ok_or(RepoError::StaleCredential)?;
        if old.revoked_at.is_some() {
            return Err(RepoError::StaleCredential);
        }
        old.revoked_at = Some(now);

        let inserted = Self::insert_credential_locked(&mut store, &self.next_cred_id, new, now)?;

        if let Some(d) = store.devices.get_mut(&device_id) {
            d.last_seen_at = Some(now);
        }

        Ok((device, inserted))
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
        if cred.revoked_at.is_some() {
            return Err(RepoError::NotFound);
        }
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
    use crate::credentials::hash_token;
    use chrono::Duration;

    async fn sample_device(repo: &MemoryDeviceRepo, public_id: &str) -> Device {
        repo.create_device(NewDevice {
            public_id: public_id.into(),
            display_name: Some("desk".into()),
            public_key: vec![1; 32],
            protocol_version_last: Some(1),
        })
        .await
        .unwrap()
    }

    fn live_cred(device_id: i64, access: &str, refresh: &str, now: DateTime<Utc>) -> NewCredential {
        NewCredential {
            device_id,
            token_hash: hash_token(access),
            refresh_token_hash: hash_token(refresh),
            access_expires_at: now + Duration::hours(24),
            expires_at: now + Duration::days(30),
        }
    }

    #[tokio::test]
    async fn create_and_lookup() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        assert_eq!(d.public_id, "1234567897");
        let found = repo.get_by_public_id("1234567897").await.unwrap().unwrap();
        assert_eq!(found.id, d.id);
    }

    #[tokio::test]
    async fn create_duplicate_public_id_conflicts() {
        let repo = MemoryDeviceRepo::new();
        sample_device(&repo, "1234567897").await;
        let err = repo
            .create_device(NewDevice {
                public_id: "1234567897".into(),
                display_name: None,
                public_key: vec![2; 32],
                protocol_version_last: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Conflict(_)));
    }

    #[tokio::test]
    async fn soft_delete_and_revoke() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        let cred = repo
            .insert_credential(live_cred(d.id, "rl_at_a", "rl_rt_b", now))
            .await
            .unwrap();
        repo.revoke_credential(cred.id, now).await.unwrap();
        assert!(repo
            .find_by_access_hash(&hash_token("rl_at_a"))
            .await
            .unwrap()
            .is_none());
        assert!(repo.soft_delete("1234567897", now).await.unwrap());
        let d2 = repo.get_by_public_id("1234567897").await.unwrap().unwrap();
        assert_eq!(d2.status, DeviceStatus::Deleted);
    }

    #[tokio::test]
    async fn find_by_refresh_hash_hit_miss_and_after_revoke() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        let cred = repo
            .insert_credential(live_cred(d.id, "rl_at_a", "rl_rt_b", now))
            .await
            .unwrap();
        let found = repo
            .find_by_refresh_hash(&hash_token("rl_rt_b"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.0.id, d.id);
        assert!(repo
            .find_by_refresh_hash(&hash_token("rl_rt_missing"))
            .await
            .unwrap()
            .is_none());
        repo.revoke_credential(cred.id, now).await.unwrap();
        assert!(repo
            .find_by_refresh_hash(&hash_token("rl_rt_b"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn revoke_all_for_device_clears_all_rows() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        repo.insert_credential(live_cred(d.id, "rl_at_1", "rl_rt_1", now))
            .await
            .unwrap();
        repo.insert_credential(live_cred(d.id, "rl_at_2", "rl_rt_2", now))
            .await
            .unwrap();
        repo.revoke_all_for_device(d.id, now).await.unwrap();
        assert!(repo
            .find_by_access_hash(&hash_token("rl_at_1"))
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .find_by_access_hash(&hash_token("rl_at_2"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn find_by_hash_still_returns_expired_rows() {
        // Expiry is enforced in handlers; repo returns non-revoked rows regardless.
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        repo.insert_credential(NewCredential {
            device_id: d.id,
            token_hash: hash_token("rl_at_exp"),
            refresh_token_hash: hash_token("rl_rt_exp"),
            access_expires_at: now - Duration::hours(1),
            expires_at: now - Duration::days(1),
        })
        .await
        .unwrap();
        let (dev, cred) = repo
            .find_by_access_hash(&hash_token("rl_at_exp"))
            .await
            .unwrap()
            .expect("expired access still findable at repo layer");
        assert_eq!(dev.id, d.id);
        assert!(cred.access_expires_at < now);
        let (_, rcred) = repo
            .find_by_refresh_hash(&hash_token("rl_rt_exp"))
            .await
            .unwrap()
            .expect("expired refresh still findable at repo layer");
        assert!(rcred.expires_at < now);
    }

    #[tokio::test]
    async fn insert_credential_unknown_device_not_found() {
        let repo = MemoryDeviceRepo::new();
        let now = Utc::now();
        let err = repo
            .insert_credential(live_cred(999, "a", "b", now))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::NotFound));
    }

    #[tokio::test]
    async fn rotate_refresh_is_atomic_and_single_use() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        repo.insert_credential(live_cred(d.id, "rl_at_old", "rl_rt_old", now))
            .await
            .unwrap();
        let new = live_cred(d.id, "rl_at_new", "rl_rt_new", now);
        let (dev, cred) = repo
            .rotate_refresh(&hash_token("rl_rt_old"), new, now)
            .await
            .unwrap();
        assert_eq!(dev.id, d.id);
        assert_eq!(cred.token_hash, hash_token("rl_at_new"));
        // Second rotate with same refresh fails
        let err = repo
            .rotate_refresh(
                &hash_token("rl_rt_old"),
                live_cred(d.id, "rl_at_x", "rl_rt_x", now),
                now,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::StaleCredential));
        // New pair is live
        assert!(repo
            .find_by_access_hash(&hash_token("rl_at_new"))
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .find_by_access_hash(&hash_token("rl_at_old"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn rotate_refresh_rejects_expired() {
        let repo = MemoryDeviceRepo::new();
        let d = sample_device(&repo, "1234567897").await;
        let now = Utc::now();
        repo.insert_credential(NewCredential {
            device_id: d.id,
            token_hash: hash_token("rl_at_e"),
            refresh_token_hash: hash_token("rl_rt_e"),
            access_expires_at: now - Duration::hours(1),
            expires_at: now - Duration::minutes(1),
        })
        .await
        .unwrap();
        let err = repo
            .rotate_refresh(&hash_token("rl_rt_e"), live_cred(d.id, "n", "m", now), now)
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::StaleCredential));
    }
}
