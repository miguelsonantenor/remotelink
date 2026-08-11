//! Host-owned blocklist and audit list HTTP APIs.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use remotelink_auth::DevicePublicId;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::credentials::hash_token;
use crate::error::{AppError, AppResult};
use crate::models::{Device, DeviceStatus};
use crate::security::{
    audit_best_effort, hash_subject, AuditEvent, AuditEventType, BlockSubjectType, BlocklistEntry,
    NewAuditEvent, NewBlocklistEntry,
};
use crate::state::AppState;

/// `POST /v1/devices/{id}/blocklist` body.
#[derive(Debug, Deserialize)]
pub struct BlocklistAddRequest {
    /// `ip` | `viewer_fingerprint` | `device`
    pub subject_type: String,
    /// Raw subject (IP address, fingerprint, or device public_id). Stored hashed.
    pub subject: String,
}

/// Blocklist list response.
#[derive(Debug, Serialize)]
pub struct BlocklistListResponse {
    pub entries: Vec<BlocklistEntryDto>,
}

/// Public blocklist entry (no raw subject).
#[derive(Debug, Serialize)]
pub struct BlocklistEntryDto {
    pub id: i64,
    pub subject_type: String,
    pub subject_hash: String,
    pub created_at: String,
}

impl From<BlocklistEntry> for BlocklistEntryDto {
    fn from(e: BlocklistEntry) -> Self {
        Self {
            id: e.id,
            subject_type: e.subject_type.as_str().to_string(),
            subject_hash: e.subject_hash,
            created_at: e.created_at.to_rfc3339(),
        }
    }
}

/// `GET .../blocklist/check` query.
#[derive(Debug, Deserialize)]
pub struct BlocklistCheckQuery {
    pub subject_type: String,
    pub subject: String,
}

/// Check response.
#[derive(Debug, Serialize)]
pub struct BlocklistCheckResponse {
    pub blocked: bool,
    pub subject_type: String,
    pub subject_hash: String,
}

/// Audit list response.
#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    pub events: Vec<AuditEventDto>,
}

#[derive(Debug, Serialize)]
pub struct AuditEventDto {
    pub id: i64,
    pub event_type: String,
    pub session_id: Option<String>,
    pub meta: serde_json::Value,
    pub created_at: String,
}

impl From<AuditEvent> for AuditEventDto {
    fn from(e: AuditEvent) -> Self {
        Self {
            id: e.id,
            event_type: e.event_type.as_str().to_string(),
            session_id: e.session_id,
            meta: e.meta,
            created_at: e.created_at.to_rfc3339(),
        }
    }
}

/// `POST /v1/devices/{id}/blocklist`
pub async fn add_blocklist(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<BlocklistAddRequest>,
) -> AppResult<(StatusCode, Json<BlocklistEntryDto>)> {
    let device = authorize_device_owner(&state, &raw_id, &headers).await?;
    let subject_type = parse_subject_type(&body.subject_type)?;
    let subject = body.subject.trim();
    if subject.is_empty() {
        return Err(AppError::BadRequest("subject is required".into()));
    }
    if subject.len() > 512 {
        return Err(AppError::BadRequest(
            "subject must be at most 512 characters".into(),
        ));
    }

    let subject_hash = hash_subject(subject);
    let entry = state
        .blocklist
        .add(NewBlocklistEntry {
            host_device_id: device.id,
            subject_type,
            subject_hash: subject_hash.clone(),
        })
        .await?;

    audit_best_effort(
        state.audit.as_ref(),
        NewAuditEvent {
            device_id: Some(device.id),
            session_id: None,
            event_type: AuditEventType::BlocklistAdd,
            meta: json!({
                "subject_type": subject_type.as_str(),
                "subject_hash": subject_hash,
                "entry_id": entry.id,
            }),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(BlocklistEntryDto::from(entry))))
}

/// `GET /v1/devices/{id}/blocklist`
pub async fn list_blocklist(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<BlocklistListResponse>> {
    let device = authorize_device_owner(&state, &raw_id, &headers).await?;
    let entries = state.blocklist.list_for_host(device.id).await?;
    Ok(Json(BlocklistListResponse {
        entries: entries.into_iter().map(BlocklistEntryDto::from).collect(),
    }))
}

/// `GET /v1/devices/{id}/blocklist/check?subject_type=&subject=`
pub async fn check_blocklist(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    Query(q): Query<BlocklistCheckQuery>,
    headers: HeaderMap,
) -> AppResult<Json<BlocklistCheckResponse>> {
    let device = authorize_device_owner(&state, &raw_id, &headers).await?;
    let subject_type = parse_subject_type(&q.subject_type)?;
    let subject = q.subject.trim();
    if subject.is_empty() {
        return Err(AppError::BadRequest("subject is required".into()));
    }
    let subject_hash = hash_subject(subject);
    let blocked = state
        .blocklist
        .is_blocked(device.id, subject_type, &subject_hash)
        .await?;
    Ok(Json(BlocklistCheckResponse {
        blocked,
        subject_type: subject_type.as_str().to_string(),
        subject_hash,
    }))
}

/// `DELETE /v1/devices/{id}/blocklist/{entry_id}`
pub async fn remove_blocklist(
    State(state): State<AppState>,
    Path((raw_id, entry_id)): Path<(String, i64)>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let device = authorize_device_owner(&state, &raw_id, &headers).await?;
    state.blocklist.remove(device.id, entry_id).await?;
    audit_best_effort(
        state.audit.as_ref(),
        NewAuditEvent {
            device_id: Some(device.id),
            session_id: None,
            event_type: AuditEventType::BlocklistRemove,
            meta: json!({ "entry_id": entry_id }),
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/devices/{id}/audit`
pub async fn list_audit(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<AuditListResponse>> {
    let device = authorize_device_owner(&state, &raw_id, &headers).await?;
    let events = state.audit.list_for_device(device.id, 100).await?;
    Ok(Json(AuditListResponse {
        events: events.into_iter().map(AuditEventDto::from).collect(),
    }))
}

async fn authorize_device_owner(
    state: &AppState,
    raw_id: &str,
    headers: &HeaderMap,
) -> AppResult<Device> {
    let public_id = DevicePublicId::parse(raw_id)
        .map(|id| id.into_string())
        .map_err(|e| AppError::BadRequest(format!("invalid device id: {e}")))?;

    let access_token = bearer_token(headers)?;
    let token_hash = hash_token(&access_token);
    let (device, cred) = state
        .repo
        .find_by_access_hash(&token_hash)
        .await?
        .ok_or(AppError::Unauthorized)?;

    if device.public_id != public_id {
        return Err(AppError::Unauthorized);
    }
    if device.status != DeviceStatus::Active {
        return Err(AppError::Unauthorized);
    }
    let now = Utc::now();
    if cred.access_expires_at < now {
        return Err(AppError::Unauthorized);
    }
    Ok(device)
}

fn parse_subject_type(raw: &str) -> AppResult<BlockSubjectType> {
    BlockSubjectType::parse(raw.trim()).ok_or_else(|| {
        AppError::BadRequest("subject_type must be one of: ip, viewer_fingerprint, device".into())
    })
}

fn bearer_token(headers: &HeaderMap) -> AppResult<String> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or(AppError::Unauthorized)?
        .to_str()
        .map_err(|_| AppError::Unauthorized)?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .ok_or(AppError::Unauthorized)?;
    if rest.is_empty() {
        return Err(AppError::Unauthorized);
    }
    Ok(rest.to_string())
}
