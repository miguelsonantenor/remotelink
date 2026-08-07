//! WebSocket signaling endpoint (`GET /v1/ws`).
//!
//! Flow (PR 5a):
//! 1. Client connects and sends `hello` (host device token; viewer token or anonymous).
//! 2. Server replies `hello_ok` and publishes host presence.
//! 3. Viewer sends `session_intent` → host receives `session_incoming` (busy lock).
//! 4. Host sends `session_accept` or `session_reject` → forwarded to viewer.
//!
//! PR 6: rate-limit session_intent, enforce host blocklist, audit accept/reject,
//! and track hello auth failures.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SessionMode, SignalMessage, PROTOCOL_VERSION,
};
use serde_json::json;

use remotelink_common::{process_registry, SessionResult};

use crate::credentials::hash_token;
use crate::models::DeviceStatus;
use crate::otp::OtpPrefilterResult;
use crate::security::{
    any_blocked, audit_best_effort, hash_subject, resolve_client_ip, AuditEventType,
    AuthAttemptTracker, BlockSubjectType, NewAuditEvent, OptionalPeer,
};
use crate::session::{default_viewer_info, ConnId, CreatePendingSession, PeerIdentity};
use crate::state::AppState;

/// `GET /v1/ws` — upgrade to the signaling WebSocket.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    OptionalPeer(peer): OptionalPeer,
) -> impl IntoResponse {
    let client_ip = resolve_client_ip(&headers, peer, state.client_ip);
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_ip))
}

async fn handle_socket(socket: WebSocket, state: AppState, client_ip: String) {
    let (conn_id, mut outbound_rx) = state.sessions.register_conn().await;
    let (mut sink, mut stream) = socket.split();

    // Forward registry → socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            match encode_message(&msg) {
                Ok(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to encode outbound signal message");
                    break;
                }
            }
        }
        let _ = sink.close().await;
    });

    // First message must be hello.
    let identity = match expect_hello(&mut stream, &state, conn_id, &client_ip).await {
        Ok(id) => id,
        Err(err_msg) => {
            let _ = state.sessions.send_to(conn_id, err_msg).await;
            // Give the writer a moment to flush, then tear down.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            state.sessions.unregister_conn(conn_id).await;
            writer.abort();
            return;
        }
    };

    // Presence already bound inside expect_hello (before hello_ok).

    // Process subsequent messages until disconnect.
    while let Some(frame) = stream.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(conn = %conn_id, error = %e, "websocket read error");
                break;
            }
        };
        match frame {
            Message::Text(text) => {
                if let Err(e) =
                    handle_text(&state, conn_id, &identity, text.as_str(), &client_ip).await
                {
                    let _ = state.sessions.send_to(conn_id, e).await;
                }
            }
            Message::Binary(_) => {
                let _ = state
                    .sessions
                    .send_to(
                        conn_id,
                        error_msg("protocol_error", "binary frames are not supported"),
                    )
                    .await;
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

    state.sessions.unregister_conn(conn_id).await;
    writer.abort();
}

async fn expect_hello(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &AppState,
    conn_id: ConnId,
    client_ip: &str,
) -> Result<PeerIdentity, SignalMessage> {
    let frame = match stream.next().await {
        Some(Ok(f)) => f,
        Some(Err(_)) | None => {
            return Err(error_msg(
                "protocol_error",
                "connection closed before hello",
            ));
        }
    };

    let text = match frame {
        Message::Text(t) => t,
        Message::Close(_) => {
            return Err(error_msg(
                "protocol_error",
                "connection closed before hello",
            ));
        }
        _ => {
            return Err(error_msg(
                "protocol_error",
                "first message must be text hello",
            ));
        }
    };

    let msg = decode_message(text.as_str())
        .map_err(|e| error_msg("protocol_error", format!("invalid hello: {e}")))?;

    let SignalMessage::Hello {
        role,
        protocol_version,
        auth,
    } = msg
    else {
        return Err(error_msg("protocol_error", "first message must be hello"));
    };

    if protocol_version == 0 || protocol_version > PROTOCOL_VERSION {
        return Err(error_msg(
            "protocol_version",
            format!(
                "unsupported protocol_version {protocol_version} (server max {PROTOCOL_VERSION})"
            ),
        ));
    }

    // Lockout before attempting auth (IP-keyed).
    let ip_key = AuthAttemptTracker::key_ip(client_ip);
    if let Err(lock) = state.auth_attempts.check_now(&ip_key) {
        return Err(error_msg("rate_limited", lock.to_string()));
    }

    let identity = match authenticate(state, role, &auth).await {
        Ok(id) => {
            state.auth_attempts.record_success(&ip_key);
            id
        }
        Err(e) => {
            // Only count unauthorized (not protocol) as auth failures.
            if matches!(&e, SignalMessage::Error { code, .. } if code == "unauthorized") {
                state.auth_attempts.record_failure_now(&ip_key);
                process_registry().inc_auth_fail();
                audit_best_effort(
                    state.audit.as_ref(),
                    NewAuditEvent {
                        device_id: None,
                        session_id: None,
                        event_type: AuditEventType::LoginFail,
                        meta: json!({
                            "ip": client_ip,
                            "role": format!("{role:?}"),
                            "via": "ws_hello",
                        }),
                    },
                )
                .await;
            }
            return Err(e);
        }
    };

    // Publish presence before hello_ok so intents cannot race a still-"offline" host.
    state.sessions.bind_peer(conn_id, identity.clone()).await;

    // hello_ok
    let ok = SignalMessage::HelloOk {
        server_time: Utc::now().to_rfc3339(),
        feature_flags: json!({
            "max_protocol_version": PROTOCOL_VERSION,
            "sdp_relay": false,
        }),
    };
    if !state.sessions.send_to(conn_id, ok).await {
        return Err(error_msg("internal", "failed to send hello_ok"));
    }

    // Touch host last_seen when authenticated as a device.
    if let (Role::Host, Some(device_id)) = (role, identity.device_id) {
        let _ = state.repo.touch_last_seen(device_id, Utc::now()).await;
    }

    Ok(identity)
}

async fn authenticate(
    state: &AppState,
    role: Role,
    auth: &HelloAuth,
) -> Result<PeerIdentity, SignalMessage> {
    let token = auth.device_token.trim();

    match role {
        Role::Host => {
            if token.is_empty() {
                return Err(error_msg(
                    "unauthorized",
                    "host hello requires device_token",
                ));
            }
            let (device, _cred) = resolve_device_token(state, token).await?;
            if device.status != DeviceStatus::Active {
                return Err(error_msg("unauthorized", "device is not active"));
            }
            Ok(PeerIdentity {
                role,
                device_public_id: Some(device.public_id),
                device_id: Some(device.id),
                anonymous: false,
            })
        }
        Role::Viewer => {
            if token.is_empty() {
                // Anonymous viewer — allowed for session_intent.
                return Ok(PeerIdentity {
                    role,
                    device_public_id: None,
                    device_id: None,
                    anonymous: true,
                });
            }

            let now = Utc::now();
            if state.sessions.validate_viewer_token(token, now).await {
                return Ok(PeerIdentity {
                    role,
                    device_public_id: None,
                    device_id: None,
                    anonymous: false,
                });
            }

            // Fall back to enrolled device access token (optional viewer device).
            match resolve_device_token(state, token).await {
                Ok((device, _)) if device.status == DeviceStatus::Active => Ok(PeerIdentity {
                    role,
                    device_public_id: Some(device.public_id),
                    device_id: Some(device.id),
                    anonymous: false,
                }),
                _ => Err(error_msg("unauthorized", "invalid viewer device_token")),
            }
        }
    }
}

async fn resolve_device_token(
    state: &AppState,
    token: &str,
) -> Result<(crate::models::Device, crate::models::DeviceCredential), SignalMessage> {
    let token_hash = hash_token(token);
    let (device, cred) = state
        .repo
        .find_by_access_hash(&token_hash)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "find_by_access_hash failed");
            error_msg("internal", "internal error")
        })?
        .ok_or_else(|| error_msg("unauthorized", "invalid device_token"))?;

    let now = Utc::now();
    if cred.access_expires_at < now {
        return Err(error_msg("unauthorized", "device_token expired"));
    }
    Ok((device, cred))
}

async fn handle_text(
    state: &AppState,
    conn_id: ConnId,
    identity: &PeerIdentity,
    text: &str,
    client_ip: &str,
) -> Result<(), SignalMessage> {
    let msg = decode_message(text)
        .map_err(|e| error_msg("protocol_error", format!("invalid message: {e}")))?;

    match msg {
        SignalMessage::Hello { .. } => Err(error_msg(
            "protocol_error",
            "hello already completed for this connection",
        )),
        SignalMessage::SessionIntent {
            session_id,
            signal_seq,
            host_public_id,
            mode,
            prefilter,
        } => {
            if identity.role != Role::Viewer {
                return Err(error_msg(
                    "unauthorized",
                    "only viewers may send session_intent",
                ));
            }
            if session_id.is_empty() {
                return Err(error_msg("bad_request", "session_id is required"));
            }
            if host_public_id.is_empty() {
                return Err(error_msg("bad_request", "host_public_id is required"));
            }

            // 1) Per-IP budget first (scanners pay here; trusted peer IP).
            if let Err(e) = state
                .rate_limits
                .session_intent
                .check_now(&format!("session_intent:ip:{client_ip}"))
            {
                return Err(error_msg("rate_limited", e.to_string()));
            }

            let host_device = state
                .repo
                .get_by_public_id(&host_public_id)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "get_by_public_id failed");
                    error_msg("internal", "internal error")
                })?
                .ok_or_else(|| error_msg("not_found", "unknown host_public_id"))?;

            if host_device.status != DeviceStatus::Active {
                return Err(error_msg("not_found", "host is not available"));
            }

            // 2) Host blocklist before charging the shared host budget.
            let mut subjects = vec![(BlockSubjectType::Ip, hash_subject(client_ip))];
            if let Some(ref viewer_pid) = identity.device_public_id {
                subjects.push((BlockSubjectType::Device, hash_subject(viewer_pid)));
                subjects.push((
                    BlockSubjectType::ViewerFingerprint,
                    hash_subject(viewer_pid),
                ));
            }
            match any_blocked(state.blocklist.as_ref(), host_device.id, &subjects).await {
                Ok(Some((ty, _))) => {
                    audit_best_effort(
                        state.audit.as_ref(),
                        NewAuditEvent {
                            device_id: Some(host_device.id),
                            session_id: Some(session_id.clone()),
                            event_type: AuditEventType::SessionIntent,
                            meta: json!({
                                "result": "blocked",
                                "subject_type": ty.as_str(),
                                "viewer_ip": client_ip,
                            }),
                        },
                    )
                    .await;
                    return Err(error_msg("blocked", "viewer is blocked by the host"));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "blocklist check failed");
                    return Err(error_msg("internal", "internal error"));
                }
            }

            // 2b) Mode A OTP prefilter when an active hash exists for the host.
            // Host-only mint (no server row) skips this gate; host re-validates later.
            if mode == SessionMode::Otp {
                if let Some(code) = prefilter
                    .get("otp")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let now = Utc::now();
                    if state.otp.active_for_host(host_device.id, now).is_some() {
                        let result = state.otp.prefilter_bind(
                            host_device.id,
                            code,
                            state.otp_pepper.as_slice(),
                            &session_id,
                            now,
                        );
                        let ok = result == OtpPrefilterResult::Ok;
                        audit_best_effort(
                            state.audit.as_ref(),
                            NewAuditEvent {
                                device_id: Some(host_device.id),
                                session_id: Some(session_id.clone()),
                                event_type: AuditEventType::OtpPrefilter,
                                meta: json!({
                                    "result": match result {
                                        OtpPrefilterResult::Ok => "ok",
                                        OtpPrefilterResult::NoActiveOtp => "no_active",
                                        OtpPrefilterResult::Reject => "reject",
                                    },
                                    "viewer_ip": client_ip,
                                }),
                            },
                        )
                        .await;
                        if !ok {
                            return Err(error_msg("auth", "OTP prefilter failed"));
                        }
                    }
                } else if state
                    .otp
                    .active_for_host(host_device.id, Utc::now())
                    .is_some()
                {
                    // Host published an OTP hash; viewer must present a code.
                    return Err(error_msg("auth", "OTP required for this host"));
                }
            }

            // 3) Host-shared budget only for non-blocked intents.
            if let Err(e) = state
                .rate_limits
                .session_intent
                .check_now(&format!("session_intent:host:{host_public_id}"))
            {
                return Err(error_msg("rate_limited", e.to_string()));
            }

            let viewer_info = default_viewer_info(identity);
            let result = state
                .sessions
                .create_pending_session(CreatePendingSession {
                    viewer_conn: conn_id,
                    session_id: session_id.clone(),
                    host_public_id,
                    host_device: host_device.clone(),
                    mode,
                    signal_seq,
                    viewer_info,
                })
                .await;

            if result.is_ok() {
                // session_id field for structured logs (EnteredSpan is !Send across await).
                tracing::info!(
                    session_id = %session_id,
                    host = %host_device.public_id,
                    "session intent pending"
                );
                audit_best_effort(
                    state.audit.as_ref(),
                    NewAuditEvent {
                        device_id: Some(host_device.id),
                        session_id: Some(session_id),
                        event_type: AuditEventType::SessionIntent,
                        meta: json!({
                            "result": "pending",
                            "viewer_ip": client_ip,
                            "viewer_device": identity.device_public_id,
                            "mode": format!("{mode:?}"),
                        }),
                    },
                )
                .await;
            }
            result
        }
        SignalMessage::SessionAccept {
            session_id,
            signal_seq,
        } => {
            if identity.role != Role::Host {
                return Err(error_msg(
                    "unauthorized",
                    "only hosts may send session_accept",
                ));
            }
            let result = state
                .sessions
                .accept_session(conn_id, &session_id, signal_seq)
                .await;
            if result.is_ok() {
                process_registry().inc_sessions(SessionResult::Accept);
                tracing::info!(session_id = %session_id, "session accepted");
                // Consume Mode A OTP bound to this session (if any).
                if let Some(device_id) = identity.device_id {
                    let _ = state
                        .otp
                        .consume_for_session(device_id, &session_id, Utc::now());
                }
                audit_best_effort(
                    state.audit.as_ref(),
                    NewAuditEvent {
                        device_id: identity.device_id,
                        session_id: Some(session_id),
                        event_type: AuditEventType::SessionAccept,
                        meta: json!({}),
                    },
                )
                .await;
            }
            result
        }
        SignalMessage::SessionReject {
            session_id,
            signal_seq,
            reason,
        } => {
            if identity.role != Role::Host {
                return Err(error_msg(
                    "unauthorized",
                    "only hosts may send session_reject",
                ));
            }
            let reason_meta = format!("{reason:?}");
            let result = state
                .sessions
                .reject_session(conn_id, &session_id, signal_seq, reason)
                .await;
            if result.is_ok() {
                process_registry().inc_sessions(SessionResult::Reject);
                tracing::info!(
                    session_id = %session_id,
                    reason = %reason_meta,
                    "session rejected"
                );
                audit_best_effort(
                    state.audit.as_ref(),
                    NewAuditEvent {
                        device_id: identity.device_id,
                        session_id: Some(session_id),
                        event_type: AuditEventType::SessionReject,
                        meta: json!({ "reason": reason_meta }),
                    },
                )
                .await;
            }
            result
        }
        // PR 5b will relay SDP/ICE; reject early with a clear code.
        SignalMessage::SessionOffer { .. }
        | SignalMessage::SessionAnswer { .. }
        | SignalMessage::IceCandidate { .. }
        | SignalMessage::MediaRestart { .. }
        | SignalMessage::Renegotiate { .. }
        | SignalMessage::AuthChallenge { .. }
        | SignalMessage::AuthResponse { .. }
        | SignalMessage::Stats { .. } => Err(error_msg(
            "not_implemented",
            "message type not handled in this server version",
        )),
        SignalMessage::SessionEnd {
            session_id,
            signal_seq,
            reason,
        } => {
            let result = state
                .sessions
                .end_session(conn_id, &session_id, signal_seq, reason.clone())
                .await;
            if result.is_ok() {
                process_registry().inc_sessions(SessionResult::End);
                tracing::info!(session_id = %session_id, %reason, "session ended");
                audit_best_effort(
                    state.audit.as_ref(),
                    NewAuditEvent {
                        device_id: identity.device_id,
                        session_id: Some(session_id),
                        event_type: AuditEventType::SessionEnd,
                        meta: json!({}),
                    },
                )
                .await;
            }
            result
        }
        SignalMessage::HelloOk { .. }
        | SignalMessage::SessionIncoming { .. }
        | SignalMessage::Error { .. } => Err(error_msg(
            "protocol_error",
            "message type is server→client only",
        )),
    }
}

fn error_msg(code: &str, message: impl Into<String>) -> SignalMessage {
    SignalMessage::Error {
        code: code.into(),
        message: message.into(),
    }
}
