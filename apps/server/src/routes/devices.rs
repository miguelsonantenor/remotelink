//! Device enrollment, token refresh, and GDPR delete.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use chrono::Utc;
use remotelink_auth::{verifying_key_from_bytes, DevicePublicId};
use remotelink_protocol::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

use crate::credentials::{hash_token, mint_tokens, refresh_expires_at};
use crate::error::{AppError, AppResult};
use crate::models::{DeviceStatus, NewCredential, NewDevice};
use crate::state::AppState;

/// Registration request body.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    /// Ed25519 verifying key, standard base64 (32 raw bytes).
    pub public_key: String,
    /// Optional friendly name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Client protocol version; defaults to current if omitted.
    #[serde(default)]
    pub protocol_version: Option<u32>,
}

/// Token pair returned on register / refresh.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: String,
    pub token_type: &'static str,
}

/// Full register response.
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub public_id: String,
    pub display_name: Option<String>,
    pub protocol_version: u32,
    #[serde(flatten)]
    pub tokens: TokenResponse,
}

/// Refresh body.
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Path parameter for device routes (public_id).
#[derive(Debug, Deserialize)]
pub struct DeleteParams {
    pub id: String,
}

/// `POST /v1/devices/register`
pub async fn register_device(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> AppResult<(StatusCode, Json<RegisterResponse>)> {
    let public_key = decode_public_key(&body.public_key)?;
    // Validate ed25519 key material.
    verifying_key_from_bytes(&public_key).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let protocol_version = body.protocol_version.unwrap_or(PROTOCOL_VERSION);
    if protocol_version == 0 || protocol_version > PROTOCOL_VERSION {
        return Err(AppError::BadRequest(format!(
            "unsupported protocol_version {protocol_version} (server max {PROTOCOL_VERSION})"
        )));
    }

    let display_name = normalize_display_name(body.display_name)?;

    // Retry public_id generation on the extremely rare collision.
    let mut device = None;
    for _ in 0..5 {
        let public_id = DevicePublicId::generate().into_string();
        match state
            .repo
            .create_device(NewDevice {
                public_id,
                display_name: display_name.clone(),
                public_key: public_key.clone(),
                protocol_version_last: Some(protocol_version as i32),
            })
            .await
        {
            Ok(d) => {
                device = Some(d);
                break;
            }
            Err(crate::repo::RepoError::Conflict(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let device = device.ok_or_else(|| {
        AppError::Internal("failed to allocate unique public_id after retries".into())
    })?;

    let now = Utc::now();
    let issued = mint_tokens(now);
    state
        .repo
        .insert_credential(NewCredential {
            device_id: device.id,
            token_hash: hash_token(&issued.access_token),
            refresh_token_hash: hash_token(&issued.refresh_token),
            expires_at: refresh_expires_at(now),
        })
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            public_id: device.public_id,
            display_name: device.display_name,
            protocol_version,
            tokens: TokenResponse {
                access_token: issued.access_token,
                refresh_token: issued.refresh_token,
                expires_at: issued.expires_at.to_rfc3339(),
                token_type: "Bearer",
            },
        }),
    ))
}

/// `POST /v1/devices/{id}/token/refresh`
pub async fn refresh_token(
    State(state): State<AppState>,
    Path(public_id): Path<String>,
    Json(body): Json<RefreshRequest>,
) -> AppResult<Json<TokenResponse>> {
    if body.refresh_token.is_empty() {
        return Err(AppError::BadRequest("refresh_token is required".into()));
    }

    let refresh_hash = hash_token(&body.refresh_token);
    let (device, cred) = state
        .repo
        .find_by_refresh_hash(&refresh_hash)
        .await?
        .ok_or(AppError::Unauthorized)?;

    if device.public_id != public_id {
        return Err(AppError::Unauthorized);
    }
    if device.status != DeviceStatus::Active {
        return Err(AppError::Unauthorized);
    }

    let now = Utc::now();
    if cred.expires_at < now {
        let _ = state.repo.revoke_credential(cred.id, now).await;
        return Err(AppError::Unauthorized);
    }

    // Rotate: revoke old, issue new.
    state.repo.revoke_credential(cred.id, now).await?;
    let issued = mint_tokens(now);
    state
        .repo
        .insert_credential(NewCredential {
            device_id: device.id,
            token_hash: hash_token(&issued.access_token),
            refresh_token_hash: hash_token(&issued.refresh_token),
            expires_at: refresh_expires_at(now),
        })
        .await?;
    let _ = state.repo.touch_last_seen(device.id, now).await;

    Ok(Json(TokenResponse {
        access_token: issued.access_token,
        refresh_token: issued.refresh_token,
        expires_at: issued.expires_at.to_rfc3339(),
        token_type: "Bearer",
    }))
}

/// `DELETE /v1/devices/{id}` — soft-delete + revoke credentials.
pub async fn delete_device(
    State(state): State<AppState>,
    Path(public_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let access_token = bearer_token(&headers)?;
    let token_hash = hash_token(&access_token);
    let (device, _cred) = state
        .repo
        .find_by_access_hash(&token_hash)
        .await?
        .ok_or(AppError::Unauthorized)?;

    if device.public_id != public_id {
        return Err(AppError::Unauthorized);
    }
    if device.status == DeviceStatus::Deleted {
        return Ok(StatusCode::NO_CONTENT);
    }

    let now = Utc::now();
    // Access tokens remain valid until expiry unless revoked; revoke all.
    state.repo.revoke_all_for_device(device.id, now).await?;
    let deleted = state.repo.soft_delete(&public_id, now).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
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

fn decode_public_key(encoded: &str) -> AppResult<Vec<u8>> {
    let trimmed = encoded.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("public_key is required".into()));
    }
    // Accept standard or URL-safe base64, with or without padding.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .map_err(|_| AppError::BadRequest("public_key must be base64-encoded".into()))?;
    if bytes.len() != 32 {
        return Err(AppError::BadRequest(format!(
            "public_key must decode to 32 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn normalize_display_name(name: Option<String>) -> AppResult<Option<String>> {
    match name {
        None => Ok(None),
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                Ok(None)
            } else if t.len() > 128 {
                Err(AppError::BadRequest(
                    "display_name must be at most 128 characters".into(),
                ))
            } else {
                Ok(Some(t.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::MemoryDeviceRepo;
    use crate::routes::router_with_repo;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use remotelink_auth::generate_device_keypair;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_app() -> axum::Router {
        router_with_repo(Arc::new(MemoryDeviceRepo::new()))
    }

    fn sample_pubkey_b64() -> String {
        let (_sk, vk) = generate_device_keypair();
        base64::engine::general_purpose::STANDARD.encode(vk.as_bytes())
    }

    async fn body_json(res: axum::response::Response) -> Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn register_refresh_delete_flow() {
        let app = test_app();
        let pk = sample_pubkey_b64();

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/devices/register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "public_key": pk,
                            "display_name": "lab-pc",
                            "protocol_version": 1
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let reg = body_json(res).await;
        let public_id = reg["public_id"].as_str().unwrap().to_string();
        assert!(DevicePublicId::is_valid(&public_id));
        assert_eq!(reg["display_name"], "lab-pc");
        let access = reg["access_token"].as_str().unwrap().to_string();
        let refresh = reg["refresh_token"].as_str().unwrap().to_string();
        assert!(access.starts_with("rl_at_"));
        assert!(refresh.starts_with("rl_rt_"));

        // Refresh
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/devices/{public_id}/token/refresh"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "refresh_token": refresh }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let tok = body_json(res).await;
        let access2 = tok["access_token"].as_str().unwrap().to_string();
        assert_ne!(access2, access);

        // Old refresh must fail
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/devices/{public_id}/token/refresh"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "refresh_token": refresh }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Delete with new access token
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/devices/{public_id}"))
                    .header("authorization", format!("Bearer {access2}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // Further refresh of rotated token should fail (device deleted / creds revoked)
        let refresh2 = tok["refresh_token"].as_str().unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/devices/{public_id}/token/refresh"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "refresh_token": refresh2 }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn register_rejects_bad_key() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/devices/register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "public_key": "not-base64!!!" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_requires_matching_bearer() {
        let app = test_app();
        let pk = sample_pubkey_b64();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/devices/register")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "public_key": pk }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let reg = body_json(res).await;
        let public_id = reg["public_id"].as_str().unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/devices/{public_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_endpoints() {
        let app = test_app();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["status"], "ok");

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["status"], "ready");
    }
}
