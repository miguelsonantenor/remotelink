//! Postgres-backed repository via sqlx.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use super::{DeviceRepository, RepoError};
use crate::models::{Device, DeviceCredential, DeviceStatus, NewCredential, NewDevice};

/// Postgres implementation of [`DeviceRepository`].
#[derive(Clone)]
pub struct PostgresDeviceRepo {
    pool: PgPool,
}

impl PostgresDeviceRepo {
    /// Connect and run embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self, RepoError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(|e| RepoError::Internal(format!("postgres connect: {e}")))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| RepoError::Internal(format!("migrate: {e}")))?;

        Ok(Self { pool })
    }

    /// Wrap an existing pool (tests / custom setup).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    fn map_device(row: &sqlx::postgres::PgRow) -> Result<Device, RepoError> {
        let status_raw: String = row
            .try_get("status")
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        let status = DeviceStatus::parse(&status_raw)
            .ok_or_else(|| RepoError::Internal(format!("unknown device status: {status_raw}")))?;
        Ok(Device {
            id: row
                .try_get("id")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            public_id: row
                .try_get("public_id")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            display_name: row
                .try_get("display_name")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            public_key: row
                .try_get("public_key")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            password_hash: row
                .try_get("password_hash")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            protocol_version_last: row
                .try_get("protocol_version_last")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            last_seen_at: row
                .try_get("last_seen_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            status,
            deleted_at: row
                .try_get("deleted_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
        })
    }

    fn map_credential(row: &sqlx::postgres::PgRow) -> Result<DeviceCredential, RepoError> {
        Ok(DeviceCredential {
            id: row
                .try_get("id")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            device_id: row
                .try_get("device_id")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            token_hash: row
                .try_get("token_hash")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            refresh_token_hash: row
                .try_get("refresh_token_hash")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            expires_at: row
                .try_get("expires_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            revoked_at: row
                .try_get("revoked_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| RepoError::Internal(e.to_string()))?,
        })
    }
}

#[async_trait]
impl DeviceRepository for PostgresDeviceRepo {
    async fn create_device(&self, new: NewDevice) -> Result<Device, RepoError> {
        let result = sqlx::query(
            r#"
            INSERT INTO devices (public_id, display_name, public_key, protocol_version_last, status)
            VALUES ($1, $2, $3, $4, 'active')
            RETURNING id, public_id, display_name, public_key, password_hash,
                      protocol_version_last, created_at, last_seen_at, status, deleted_at
            "#,
        )
        .bind(&new.public_id)
        .bind(&new.display_name)
        .bind(&new.public_key)
        .bind(new.protocol_version_last)
        .fetch_one(&self.pool)
        .await;

        match result {
            Ok(row) => Self::map_device(&row),
            Err(sqlx::Error::Database(db)) if db.constraint() == Some("devices_public_id_key") => {
                Err(RepoError::Conflict(format!(
                    "public_id {} already exists",
                    new.public_id
                )))
            }
            Err(e) => Err(RepoError::Internal(e.to_string())),
        }
    }

    async fn get_by_public_id(&self, public_id: &str) -> Result<Option<Device>, RepoError> {
        let row = sqlx::query(
            r#"
            SELECT id, public_id, display_name, public_key, password_hash,
                   protocol_version_last, created_at, last_seen_at, status, deleted_at
            FROM devices WHERE public_id = $1
            "#,
        )
        .bind(public_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        row.map(|r| Self::map_device(&r)).transpose()
    }

    async fn get_by_id(&self, id: i64) -> Result<Option<Device>, RepoError> {
        let row = sqlx::query(
            r#"
            SELECT id, public_id, display_name, public_key, password_hash,
                   protocol_version_last, created_at, last_seen_at, status, deleted_at
            FROM devices WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        row.map(|r| Self::map_device(&r)).transpose()
    }

    async fn soft_delete(&self, public_id: &str, at: DateTime<Utc>) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            UPDATE devices
            SET status = 'deleted', deleted_at = $2
            WHERE public_id = $1 AND status <> 'deleted'
            "#,
        )
        .bind(public_id)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        if result.rows_affected() > 0 {
            return Ok(true);
        }
        // Already deleted or missing
        Ok(self.get_by_public_id(public_id).await?.is_some())
    }

    async fn insert_credential(&self, new: NewCredential) -> Result<DeviceCredential, RepoError> {
        let row = sqlx::query(
            r#"
            INSERT INTO device_credentials
                (device_id, token_hash, refresh_token_hash, expires_at)
            VALUES ($1, $2, $3, $4)
            RETURNING id, device_id, token_hash, refresh_token_hash,
                      expires_at, revoked_at, created_at
            "#,
        )
        .bind(new.device_id)
        .bind(&new.token_hash)
        .bind(&new.refresh_token_hash)
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        Self::map_credential(&row)
    }

    async fn find_by_access_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError> {
        let row = sqlx::query(
            r#"
            SELECT
                d.id AS d_id, d.public_id, d.display_name, d.public_key, d.password_hash,
                d.protocol_version_last, d.created_at AS d_created_at, d.last_seen_at,
                d.status, d.deleted_at,
                c.id AS c_id, c.device_id, c.token_hash, c.refresh_token_hash,
                c.expires_at, c.revoked_at, c.created_at AS c_created_at
            FROM device_credentials c
            JOIN devices d ON d.id = c.device_id
            WHERE c.token_hash = $1 AND c.revoked_at IS NULL
            "#,
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        row.map(|r| join_row_to_pair(&r)).transpose()
    }

    async fn find_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<(Device, DeviceCredential)>, RepoError> {
        let row = sqlx::query(
            r#"
            SELECT
                d.id AS d_id, d.public_id, d.display_name, d.public_key, d.password_hash,
                d.protocol_version_last, d.created_at AS d_created_at, d.last_seen_at,
                d.status, d.deleted_at,
                c.id AS c_id, c.device_id, c.token_hash, c.refresh_token_hash,
                c.expires_at, c.revoked_at, c.created_at AS c_created_at
            FROM device_credentials c
            JOIN devices d ON d.id = c.device_id
            WHERE c.refresh_token_hash = $1 AND c.revoked_at IS NULL
            "#,
        )
        .bind(refresh_token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        row.map(|r| join_row_to_pair(&r)).transpose()
    }

    async fn revoke_credential(
        &self,
        credential_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            UPDATE device_credentials SET revoked_at = $2
            WHERE id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(credential_id)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NotFound);
        }
        Ok(())
    }

    async fn revoke_all_for_device(
        &self,
        device_id: i64,
        at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            UPDATE device_credentials SET revoked_at = $2
            WHERE device_id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(device_id)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn touch_last_seen(&self, device_id: i64, at: DateTime<Utc>) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            UPDATE devices SET last_seen_at = $2 WHERE id = $1
            "#,
        )
        .bind(device_id)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NotFound);
        }
        Ok(())
    }

    async fn ping(&self) -> Result<(), RepoError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(())
    }
}

fn join_row_to_pair(row: &sqlx::postgres::PgRow) -> Result<(Device, DeviceCredential), RepoError> {
    let status_raw: String = row
        .try_get("status")
        .map_err(|e| RepoError::Internal(e.to_string()))?;
    let status = DeviceStatus::parse(&status_raw)
        .ok_or_else(|| RepoError::Internal(format!("unknown device status: {status_raw}")))?;

    let device = Device {
        id: row
            .try_get("d_id")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        public_id: row
            .try_get("public_id")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        display_name: row
            .try_get("display_name")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        public_key: row
            .try_get("public_key")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        password_hash: row
            .try_get("password_hash")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        protocol_version_last: row
            .try_get("protocol_version_last")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        created_at: row
            .try_get("d_created_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        last_seen_at: row
            .try_get("last_seen_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        status,
        deleted_at: row
            .try_get("deleted_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
    };

    let cred = DeviceCredential {
        id: row
            .try_get("c_id")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        device_id: row
            .try_get("device_id")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        token_hash: row
            .try_get("token_hash")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        refresh_token_hash: row
            .try_get("refresh_token_hash")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        expires_at: row
            .try_get("expires_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        revoked_at: row
            .try_get("revoked_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
        created_at: row
            .try_get("c_created_at")
            .map_err(|e| RepoError::Internal(e.to_string()))?,
    };

    Ok((device, cred))
}
