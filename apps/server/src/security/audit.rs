//! Audit event store (in-memory + trait; Postgres path via migration).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Well-known audit event types for register / auth / session / delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    Register,
    LoginFail,
    LoginSuccess,
    SessionIntent,
    SessionAccept,
    SessionReject,
    SessionEnd,
    Delete,
    BlocklistAdd,
    BlocklistRemove,
    /// Host minted a Mode A OTP hash (plaintext never logged).
    OtpMint,
    /// OTP prefilter on session_intent succeeded or failed.
    OtpPrefilter,
}

impl AuditEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::LoginFail => "login_fail",
            Self::LoginSuccess => "login_success",
            Self::SessionIntent => "session_intent",
            Self::SessionAccept => "session_accept",
            Self::SessionReject => "session_reject",
            Self::SessionEnd => "session_end",
            Self::Delete => "delete",
            Self::BlocklistAdd => "blocklist_add",
            Self::BlocklistRemove => "blocklist_remove",
            Self::OtpMint => "otp_mint",
            Self::OtpPrefilter => "otp_prefilter",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "register" => Some(Self::Register),
            "login_fail" => Some(Self::LoginFail),
            "login_success" => Some(Self::LoginSuccess),
            "session_intent" => Some(Self::SessionIntent),
            "session_accept" => Some(Self::SessionAccept),
            "session_reject" => Some(Self::SessionReject),
            "session_end" => Some(Self::SessionEnd),
            "delete" => Some(Self::Delete),
            "blocklist_add" => Some(Self::BlocklistAdd),
            "blocklist_remove" => Some(Self::BlocklistRemove),
            "otp_mint" => Some(Self::OtpMint),
            "otp_prefilter" => Some(Self::OtpPrefilter),
            _ => None,
        }
    }
}

/// Persisted audit row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditEvent {
    pub id: i64,
    pub device_id: Option<i64>,
    pub session_id: Option<String>,
    pub event_type: AuditEventType,
    pub meta: Value,
    pub created_at: DateTime<Utc>,
}

/// Fields required to append an event.
#[derive(Debug, Clone)]
pub struct NewAuditEvent {
    pub device_id: Option<i64>,
    pub session_id: Option<String>,
    pub event_type: AuditEventType,
    pub meta: Value,
}

/// Audit storage errors.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("internal: {0}")]
    Internal(String),
}

/// Async audit log sink.
#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn append(&self, event: NewAuditEvent) -> Result<AuditEvent, AuditError>;

    /// Newest-first events for a device (and global events with that device_id).
    async fn list_for_device(
        &self,
        device_id: i64,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, AuditError>;
}

/// Thread-safe in-memory audit log (tests / single-node).
#[derive(Debug, Default)]
pub struct MemoryAuditStore {
    next_id: AtomicI64,
    events: Mutex<Vec<AuditEvent>>,
    /// Index: device_id → event ids (for faster list).
    by_device: Mutex<HashMap<i64, Vec<i64>>>,
}

impl MemoryAuditStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total events stored (tests).
    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl AuditStore for MemoryAuditStore {
    async fn append(&self, event: NewAuditEvent) -> Result<AuditEvent, AuditError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let row = AuditEvent {
            id,
            device_id: event.device_id,
            session_id: event.session_id,
            event_type: event.event_type,
            meta: event.meta,
            created_at: Utc::now(),
        };

        {
            let mut events = self
                .events
                .lock()
                .map_err(|_| AuditError::Internal("audit store poisoned".into()))?;
            events.push(row.clone());
        }

        if let Some(device_id) = row.device_id {
            let mut by = self
                .by_device
                .lock()
                .map_err(|_| AuditError::Internal("audit store poisoned".into()))?;
            by.entry(device_id).or_default().push(id);
        }

        Ok(row)
    }

    async fn list_for_device(
        &self,
        device_id: i64,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, AuditError> {
        let events = self
            .events
            .lock()
            .map_err(|_| AuditError::Internal("audit store poisoned".into()))?;
        let mut matched: Vec<AuditEvent> = events
            .iter()
            .filter(|e| e.device_id == Some(device_id))
            .cloned()
            .collect();
        matched.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        matched.truncate(limit.clamp(1, 500));
        Ok(matched)
    }
}

/// Best-effort append that logs on failure (handlers should not fail solely on audit).
pub async fn audit_best_effort(store: &dyn AuditStore, event: NewAuditEvent) {
    if let Err(e) = store.append(event).await {
        tracing::warn!(error = %e, "failed to append audit event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn append_and_list() {
        let store = MemoryAuditStore::new();
        store
            .append(NewAuditEvent {
                device_id: Some(1),
                session_id: None,
                event_type: AuditEventType::Register,
                meta: json!({ "ip": "10.0.0.1" }),
            })
            .await
            .unwrap();
        store
            .append(NewAuditEvent {
                device_id: Some(1),
                session_id: Some("s1".into()),
                event_type: AuditEventType::SessionAccept,
                meta: json!({}),
            })
            .await
            .unwrap();
        store
            .append(NewAuditEvent {
                device_id: Some(2),
                session_id: None,
                event_type: AuditEventType::Delete,
                meta: json!({}),
            })
            .await
            .unwrap();

        let list = store.list_for_device(1, 10).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].event_type, AuditEventType::SessionAccept);
        assert_eq!(list[1].event_type, AuditEventType::Register);
    }
}
