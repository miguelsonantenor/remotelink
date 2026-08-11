//! Operator admin routes (force-disconnect, etc.).
//!
//! Auth: `Authorization: Bearer <token>` or `X-Admin-Token` header, compared to
//! the server `ADMIN_TOKEN` env (stored on [`AppState::admin_token`]). When the
//! token is unset/empty/too short, all admin calls return 401.
//!
//! Abuse controls (PR 6 style):
//! - IP-keyed token-bucket via [`RateLimiters::admin`]
//! - [`AuthAttemptTracker`] lockout on failed admin auth
//! - Audit `login_fail` on bad token (never logs the provided secret)

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::security::{
    audit_best_effort, resolve_client_ip, AuditEventType, AuthAttemptTracker, NewAuditEvent,
    OptionalPeer,
};
use crate::state::AppState;

/// Successful force-disconnect response.
#[derive(Debug, Serialize)]
pub struct ForceDisconnectResponse {
    pub session_id: String,
    pub status: &'static str,
    pub reason: &'static str,
}

/// `POST /v1/admin/sessions/{id}/force-disconnect`
///
/// Closes a pending or active session and broadcasts
/// `session_end` with `reason=security` to both peers.
pub async fn force_disconnect(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    OptionalPeer(peer): OptionalPeer,
) -> AppResult<(StatusCode, Json<ForceDisconnectResponse>)> {
    let ip = resolve_client_ip(&headers, peer, state.client_ip);

    if let Err(e) = state.rate_limits.admin.check_now(&format!("admin:{ip}")) {
        return Err(AppError::rate_limited(e.to_string(), e.retry_after));
    }

    authorize_admin(&state, &headers, &ip).await?;

    let sid = session_id.trim();
    if sid.is_empty() {
        return Err(AppError::BadRequest("session_id is required".into()));
    }
    if sid.len() > 128 {
        return Err(AppError::BadRequest(
            "session_id must be at most 128 characters".into(),
        ));
    }

    let closed = state
        .sessions
        .force_disconnect(sid, "security")
        .await
        .map_err(AppError::Internal)?;

    if !closed {
        return Err(AppError::NotFound);
    }

    audit_best_effort(
        state.audit.as_ref(),
        NewAuditEvent {
            device_id: None,
            session_id: Some(sid.to_string()),
            event_type: AuditEventType::SessionEnd,
            meta: json!({
                "reason": "security",
                "source": "admin_force_disconnect",
                "ip": ip,
            }),
        },
    )
    .await;

    Ok((
        StatusCode::OK,
        Json(ForceDisconnectResponse {
            session_id: sid.to_string(),
            status: "disconnected",
            reason: "security",
        }),
    ))
}

/// Validate admin bearer / header against configured token.
///
/// Failed auth is rate-limited via lockout keys and audited (no secret logged).
async fn authorize_admin(state: &AppState, headers: &HeaderMap, ip: &str) -> AppResult<()> {
    let auth_key = AuthAttemptTracker::key_ip(&format!("admin:{ip}"));
    if let Err(lock) = state.auth_attempts.check_now(&auth_key) {
        return Err(AppError::rate_limited(lock.to_string(), lock.retry_after));
    }

    let Some(expected) = state.admin_token.as_deref() else {
        // Unconfigured admin surface: still count as failed probe for abuse.
        record_admin_auth_failure(state, ip, "admin_token_unset").await;
        return Err(AppError::Unauthorized);
    };
    if expected.is_empty() {
        record_admin_auth_failure(state, ip, "admin_token_empty").await;
        return Err(AppError::Unauthorized);
    }

    let provided = extract_admin_token(headers);
    match provided {
        Some(got) if subtle_eq(got.as_bytes(), expected.as_bytes()) => {
            state.auth_attempts.record_success(&auth_key);
            Ok(())
        }
        Some(_) => {
            record_admin_auth_failure(state, ip, "bad_token").await;
            Err(AppError::Unauthorized)
        }
        None => {
            record_admin_auth_failure(state, ip, "missing_token").await;
            Err(AppError::Unauthorized)
        }
    }
}

async fn record_admin_auth_failure(state: &AppState, ip: &str, reason: &str) {
    let auth_key = AuthAttemptTracker::key_ip(&format!("admin:{ip}"));
    state.auth_attempts.record_failure_now(&auth_key);
    // Never log the provided secret — only reason + IP.
    tracing::warn!(%ip, %reason, "admin auth failed");
    audit_best_effort(
        state.audit.as_ref(),
        NewAuditEvent {
            device_id: None,
            session_id: None,
            event_type: AuditEventType::LoginFail,
            meta: json!({
                "via": "admin",
                "ip": ip,
                "reason": reason,
            }),
        },
    )
    .await;
}

fn extract_admin_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-admin-token").and_then(|v| v.to_str().ok()) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Constant-time-ish compare for equal-length secrets; rejects length mismatch.
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Device, DeviceStatus};
    use crate::repo::MemoryDeviceRepo;
    use crate::routes::router;
    use crate::security::{
        AuthAttemptConfig, AuthAttemptTracker, ClientIpConfig, MemoryAuditStore, MemoryBlocklist,
        RateLimitConfig, RateLimiters,
    };
    use crate::session::{CreatePendingSession, SessionRegistry};
    use axum::body::Body;
    use axum::http::Request;
    use chrono::Utc;
    use http_body_util::BodyExt;
    use remotelink_protocol::SessionMode;
    use serde_json::Value;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    /// Tokens must meet [`AppState::ADMIN_TOKEN_MIN_LEN`].
    const ADMIN: &str = "secret-admin-token!";
    const ADMIN_HDR: &str = "hdr-admin-token!!";

    fn admin_state(token: Option<&str>) -> AppState {
        let mut state = AppState::new(Arc::new(MemoryDeviceRepo::new()));
        if let Some(t) = token {
            state = state.with_admin_token(t);
        }
        state
    }

    async fn body_json(res: axum::response::Response) -> Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn seed_session(sessions: &SessionRegistry, session_id: &str) {
        let (host_conn, _host_rx) = sessions.register_conn().await;
        let (viewer_conn, _viewer_rx) = sessions.register_conn().await;
        sessions
            .bind_peer(
                host_conn,
                crate::session::PeerIdentity {
                    role: remotelink_protocol::Role::Host,
                    device_public_id: Some("12345678903".into()),
                    device_id: Some(1),
                    anonymous: false,
                },
            )
            .await;
        sessions
            .bind_peer(
                viewer_conn,
                crate::session::PeerIdentity {
                    role: remotelink_protocol::Role::Viewer,
                    device_public_id: None,
                    device_id: None,
                    anonymous: true,
                },
            )
            .await;

        let device = Device {
            id: 1,
            public_id: "12345678903".into(),
            display_name: Some("host".into()),
            public_key: vec![0u8; 32],
            password_hash: None,
            protocol_version_last: Some(1),
            created_at: Utc::now(),
            last_seen_at: None,
            status: DeviceStatus::Active,
            deleted_at: None,
        };

        sessions
            .create_pending_session(CreatePendingSession {
                viewer_conn,
                session_id: session_id.into(),
                host_public_id: "12345678903".into(),
                host_device: device,
                mode: SessionMode::Otp,
                signal_seq: 1,
                viewer_info: json!({}),
            })
            .await
            .expect("create pending");
    }

    #[tokio::test]
    async fn force_disconnect_requires_token() {
        let state = admin_state(None);
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/s1/force-disconnect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn force_disconnect_rejects_bad_token() {
        let state = admin_state(Some(ADMIN));
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/s1/force-disconnect")
                    .header("authorization", "Bearer wrong-token-value")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn force_disconnect_not_found() {
        let state = admin_state(Some(ADMIN));
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/missing-sess/force-disconnect")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn force_disconnect_ok_bearer() {
        let sessions = Arc::new(SessionRegistry::new());
        seed_session(&sessions, "sess-force-1").await;
        assert!(sessions.session_state("sess-force-1").await.is_some());

        let state = AppState::with_sessions(Arc::new(MemoryDeviceRepo::new()), sessions.clone())
            .with_admin_token(ADMIN);
        let app = router(state);

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/sess-force-1/force-disconnect")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["session_id"], "sess-force-1");
        assert_eq!(body["status"], "disconnected");
        assert_eq!(body["reason"], "security");
        assert!(sessions.session_state("sess-force-1").await.is_none());
        assert!(sessions
            .busy_session_for_host("12345678903")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn force_disconnect_ok_x_admin_header() {
        let sessions = Arc::new(SessionRegistry::new());
        seed_session(&sessions, "sess-force-2").await;

        let state = AppState::with_sessions(Arc::new(MemoryDeviceRepo::new()), sessions.clone())
            .with_admin_token(ADMIN_HDR);
        let app = router(state);

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/sess-force-2/force-disconnect")
                    .header("x-admin-token", ADMIN_HDR)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(sessions.session_state("sess-force-2").await.is_none());
    }

    #[tokio::test]
    async fn force_disconnect_rate_limited() {
        let rate_limits = Arc::new(
            RateLimiters::with_configs(
                RateLimitConfig::per_window(100, Duration::from_secs(60)),
                RateLimitConfig::per_window(100, Duration::from_secs(60)),
                RateLimitConfig::per_window(100, Duration::from_secs(60)),
            )
            .with_admin(RateLimitConfig {
                capacity: 2.0,
                refill_per_sec: 0.0,
            }),
        );
        let state = AppState::with_security(
            Arc::new(MemoryDeviceRepo::new()),
            Arc::new(SessionRegistry::new()),
            rate_limits,
            Arc::new(AuthAttemptTracker::with_defaults()),
            Arc::new(MemoryAuditStore::new()),
            Arc::new(MemoryBlocklist::new()),
        )
        .with_admin_token(ADMIN)
        .with_client_ip(ClientIpConfig { trust_proxy: true });
        let app = router(state);

        for _ in 0..2 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/admin/sessions/missing/force-disconnect")
                        .header("authorization", format!("Bearer {ADMIN}"))
                        .header("x-forwarded-for", "198.51.100.10")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            // Authorized but session missing → 404 still consumes admin budget.
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/missing/force-disconnect")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .header("x-forwarded-for", "198.51.100.10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().get("retry-after").is_some());
    }

    #[tokio::test]
    async fn force_disconnect_auth_failure_audited_and_lockout() {
        let auth = Arc::new(AuthAttemptTracker::new(AuthAttemptConfig {
            max_failures_before_lockout: 3,
            base_lockout: Duration::from_secs(60),
            max_lockout: Duration::from_secs(60),
            failure_window: Duration::from_secs(600),
        }));
        let audit = Arc::new(MemoryAuditStore::new());
        let state = AppState::with_security(
            Arc::new(MemoryDeviceRepo::new()),
            Arc::new(SessionRegistry::new()),
            Arc::new(RateLimiters::new()),
            auth.clone(),
            audit.clone(),
            Arc::new(MemoryBlocklist::new()),
        )
        .with_admin_token(ADMIN)
        .with_client_ip(ClientIpConfig { trust_proxy: true });
        let app = router(state);

        for _ in 0..3 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/admin/sessions/s1/force-disconnect")
                        .header("authorization", "Bearer not-the-admin-token")
                        .header("x-forwarded-for", "203.0.113.77")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        }

        // Next attempt locked out.
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/sessions/s1/force-disconnect")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .header("x-forwarded-for", "203.0.113.77")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);

        let events = audit.events_snapshot();
        let fails: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == AuditEventType::LoginFail)
            .collect();
        assert!(
            fails.len() >= 3,
            "expected audited admin auth failures, got {fails:?}"
        );
        assert_eq!(fails[0].meta["via"], "admin");
        assert_eq!(fails[0].meta["reason"], "bad_token");
        // Ensure no secret material was stored.
        let meta_s = fails[0].meta.to_string();
        assert!(!meta_s.contains(ADMIN));
        assert!(!meta_s.contains("not-the-admin-token"));
    }

    #[test]
    fn short_admin_token_rejected() {
        let state = AppState::new(Arc::new(MemoryDeviceRepo::new())).with_admin_token("short");
        assert!(state.admin_token.is_none());
    }

    #[test]
    fn subtle_eq_works() {
        assert!(subtle_eq(b"abc", b"abc"));
        assert!(!subtle_eq(b"abc", b"abd"));
        assert!(!subtle_eq(b"abc", b"ab"));
    }

    #[test]
    fn extract_bearer() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer tok123".parse().unwrap());
        assert_eq!(extract_admin_token(&h).as_deref(), Some("tok123"));
    }
}
