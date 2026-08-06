//! WebSocket signaling endpoint (`GET /v1/ws`).
//!
//! Flow:
//! 1. Client connects and sends `hello` (host device token; viewer token or anonymous).
//! 2. Server replies `hello_ok` and publishes host presence.
//! 3. Viewer sends `session_intent` → host receives `session_incoming` (busy lock).
//! 4. Host sends `session_accept` or `session_reject` → forwarded to viewer.
//! 5. After accept: `session_offer` / `session_answer` / `ice_candidate` (and optional
//!    `media_restart` / `renegotiate`) are forward-only relayed between parties.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SignalMessage, PROTOCOL_VERSION,
};
use serde_json::json;

use crate::credentials::hash_token;
use crate::models::DeviceStatus;
use crate::session::{default_viewer_info, ConnId, CreatePendingSession, PeerIdentity};
use crate::state::AppState;

/// `GET /v1/ws` — upgrade to the signaling WebSocket.
pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
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
    let identity = match expect_hello(&mut stream, &state, conn_id).await {
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
                if let Err(e) = handle_text(&state, conn_id, &identity, text.as_str()).await {
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

    let identity = authenticate(state, role, &auth).await?;

    // Publish presence before hello_ok so intents cannot race a still-"offline" host.
    state.sessions.bind_peer(conn_id, identity.clone()).await;

    // hello_ok
    let ok = SignalMessage::HelloOk {
        server_time: Utc::now().to_rfc3339(),
        feature_flags: json!({
            "max_protocol_version": PROTOCOL_VERSION,
            "sdp_relay": true,
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
            prefilter: _,
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

            let viewer_info = default_viewer_info(identity);
            state
                .sessions
                .create_pending_session(CreatePendingSession {
                    viewer_conn: conn_id,
                    session_id,
                    host_public_id,
                    host_device,
                    mode,
                    signal_seq,
                    viewer_info,
                })
                .await
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
            state
                .sessions
                .accept_session(conn_id, &session_id, signal_seq)
                .await
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
            state
                .sessions
                .reject_session(conn_id, &session_id, signal_seq, reason)
                .await
        }
        // Forward-only SDP/ICE/media control between session parties (active only).
        // Size limits are enforced by decode_message (MAX_SDP_BYTES, MAX_ICE_*, etc.).
        msg @ (SignalMessage::SessionOffer { .. }
        | SignalMessage::SessionAnswer { .. }
        | SignalMessage::IceCandidate { .. }
        | SignalMessage::MediaRestart { .. }
        | SignalMessage::Renegotiate { .. }) => {
            state.sessions.relay_session_message(conn_id, msg).await
        }
        SignalMessage::SessionEnd {
            session_id,
            signal_seq,
            reason,
        } => {
            state
                .sessions
                .end_session(conn_id, &session_id, signal_seq, reason)
                .await
        }
        // Auth challenge/response and stats relay land in a later PR.
        SignalMessage::AuthChallenge { .. }
        | SignalMessage::AuthResponse { .. }
        | SignalMessage::Stats { .. } => Err(error_msg(
            "not_implemented",
            "message type not handled in this server version",
        )),
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
