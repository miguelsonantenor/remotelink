//! Integration tests for `/v1/ws` hello + session_intent + accept/reject.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_auth::DevicePublicId;
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, IceCandidate, RejectReason, Role, SessionMode,
    SignalMessage, MAX_SDP_BYTES, PROTOCOL_VERSION,
};
use remotelink_server::credentials::{hash_token, mint_tokens, new_credential_from_issued};
use remotelink_server::{
    router, AppState, DeviceRepository, DeviceStatus, MemoryDeviceRepo, NewCredential, NewDevice,
    SessionRegistry,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_server(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Tiny yield so the accept loop is ready.
    tokio::task::yield_now().await;
    addr
}

async fn register_host(repo: &MemoryDeviceRepo) -> (String, String) {
    let public_id = DevicePublicId::generate().into_string();
    let device = repo
        .create_device(NewDevice {
            public_id: public_id.clone(),
            display_name: Some("ws-host".into()),
            public_key: vec![7; 32],
            protocol_version_last: Some(1),
        })
        .await
        .unwrap();
    let now = Utc::now();
    let issued = mint_tokens(now);
    repo.insert_credential(new_credential_from_issued(device.id, &issued, now))
        .await
        .unwrap();
    (public_id, issued.access_token)
}

async fn connect_ws(addr: SocketAddr) -> WsStream {
    let url = format!("ws://{addr}/v1/ws");
    let (ws, _) = connect_async(url).await.expect("ws connect");
    ws
}

async fn send_msg(ws: &mut WsStream, msg: &SignalMessage) {
    let text = encode_message(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

async fn recv_msg(ws: &mut WsStream) -> SignalMessage {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = ws.next().await.expect("ws closed").expect("ws error");
            match frame {
                Message::Text(t) => return decode_message(t.as_str()).expect("decode"),
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await.ok();
                }
                Message::Pong(_) => {}
                Message::Close(_) => panic!("unexpected close"),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for message")
}

fn hello_host(token: &str) -> SignalMessage {
    SignalMessage::Hello {
        role: Role::Host,
        protocol_version: PROTOCOL_VERSION,
        auth: HelloAuth {
            device_token: token.into(),
        },
    }
}

fn hello_viewer(token: &str) -> SignalMessage {
    SignalMessage::Hello {
        role: Role::Viewer,
        protocol_version: PROTOCOL_VERSION,
        auth: HelloAuth {
            device_token: token.into(),
        },
    }
}

#[tokio::test]
async fn host_and_anonymous_viewer_hello_ok() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions);
    let (public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    match recv_msg(&mut host).await {
        SignalMessage::HelloOk { server_time, .. } => {
            assert!(!server_time.is_empty());
        }
        other => panic!("expected hello_ok, got {other:?}"),
    }

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    match recv_msg(&mut viewer).await {
        SignalMessage::HelloOk { .. } => {}
        other => panic!("expected hello_ok, got {other:?}"),
    }

    // Host identity bound (public_id used in later tests).
    assert!(!public_id.is_empty());
}

#[tokio::test]
async fn host_hello_rejects_bad_token() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo);
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host("rl_at_notarealtoken")).await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "unauthorized"),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn host_hello_rejects_unsupported_protocol_version() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let (_public_id, access) = register_host(&repo).await;
    let state = AppState::new(repo);
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(
        &mut host,
        &SignalMessage::Hello {
            role: Role::Host,
            protocol_version: 99,
            auth: HelloAuth {
                device_token: access,
            },
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "protocol_version"),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn session_intent_accept_flow() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));

    let session_id = "sess-accept-1";
    send_msg(
        &mut viewer,
        &SignalMessage::SessionIntent {
            session_id: session_id.into(),
            signal_seq: 1,
            host_public_id: host_public_id.clone(),
            mode: SessionMode::Otp,
            prefilter: json!({}),
        },
    )
    .await;

    match recv_msg(&mut host).await {
        SignalMessage::SessionIncoming {
            session_id: sid,
            signal_seq,
            viewer_info,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 2);
            assert_eq!(viewer_info["anonymous"], true);
        }
        other => panic!("expected session_incoming, got {other:?}"),
    }

    assert_eq!(
        sessions
            .busy_session_for_host(&host_public_id)
            .await
            .as_deref(),
        Some(session_id)
    );
    assert_eq!(
        sessions.session_state(session_id).await,
        Some(remotelink_server::SessionState::Pending)
    );

    send_msg(
        &mut host,
        &SignalMessage::SessionAccept {
            session_id: session_id.into(),
            signal_seq: 3,
        },
    )
    .await;

    match recv_msg(&mut viewer).await {
        SignalMessage::SessionAccept {
            session_id: sid,
            signal_seq,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 3);
        }
        other => panic!("expected session_accept, got {other:?}"),
    }

    assert_eq!(
        sessions.session_state(session_id).await,
        Some(remotelink_server::SessionState::Active)
    );
    // Busy lock held while active.
    assert_eq!(
        sessions
            .busy_session_for_host(&host_public_id)
            .await
            .as_deref(),
        Some(session_id)
    );
}

#[tokio::test]
async fn session_intent_reject_flow() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));

    let session_id = "sess-reject-1";
    send_msg(
        &mut viewer,
        &SignalMessage::SessionIntent {
            session_id: session_id.into(),
            signal_seq: 1,
            host_public_id: host_public_id.clone(),
            mode: SessionMode::Password,
            prefilter: json!({}),
        },
    )
    .await;

    match recv_msg(&mut host).await {
        SignalMessage::SessionIncoming { .. } => {}
        other => panic!("expected session_incoming, got {other:?}"),
    }

    send_msg(
        &mut host,
        &SignalMessage::SessionReject {
            session_id: session_id.into(),
            signal_seq: 3,
            reason: RejectReason::Policy,
        },
    )
    .await;

    match recv_msg(&mut viewer).await {
        SignalMessage::SessionReject {
            session_id: sid,
            reason,
            ..
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(reason, RejectReason::Policy);
        }
        other => panic!("expected session_reject, got {other:?}"),
    }

    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_none());
    assert!(sessions.session_state(session_id).await.is_none());
}

#[tokio::test]
async fn busy_lock_rejects_second_intent() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions);
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut v1 = connect_ws(addr).await;
    send_msg(&mut v1, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut v1).await,
        SignalMessage::HelloOk { .. }
    ));

    send_msg(
        &mut v1,
        &SignalMessage::SessionIntent {
            session_id: "sess-busy-1".into(),
            signal_seq: 1,
            host_public_id: host_public_id.clone(),
            mode: SessionMode::Unattended,
            prefilter: json!({}),
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));

    let mut v2 = connect_ws(addr).await;
    send_msg(&mut v2, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut v2).await,
        SignalMessage::HelloOk { .. }
    ));

    send_msg(
        &mut v2,
        &SignalMessage::SessionIntent {
            session_id: "sess-busy-2".into(),
            signal_seq: 1,
            host_public_id,
            mode: SessionMode::Otp,
            prefilter: json!({}),
        },
    )
    .await;

    match recv_msg(&mut v2).await {
        SignalMessage::SessionReject {
            reason, session_id, ..
        } => {
            assert_eq!(reason, RejectReason::Busy);
            assert_eq!(session_id, "sess-busy-2");
        }
        other => panic!("expected busy reject, got {other:?}"),
    }
}

#[tokio::test]
async fn intent_when_host_offline() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo.clone());
    let (host_public_id, _access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));

    send_msg(
        &mut viewer,
        &SignalMessage::SessionIntent {
            session_id: "sess-offline".into(),
            signal_seq: 1,
            host_public_id,
            mode: SessionMode::Otp,
            prefilter: json!({}),
        },
    )
    .await;

    match recv_msg(&mut viewer).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "host_offline"),
        other => panic!("expected host_offline error, got {other:?}"),
    }
}

#[tokio::test]
async fn viewer_session_token_hello() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let token = sessions.mint_viewer_token(chrono::Utc::now()).await;
    let state = AppState::with_sessions(repo, sessions);
    let addr = spawn_server(state).await;

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer(&token)).await;
    match recv_msg(&mut viewer).await {
        SignalMessage::HelloOk { .. } => {}
        other => panic!("expected hello_ok with viewer token, got {other:?}"),
    }
}

#[tokio::test]
async fn signal_seq_present_on_session_messages() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));

    send_msg(
        &mut viewer,
        &SignalMessage::SessionIntent {
            session_id: "sess-seq".into(),
            signal_seq: 10,
            host_public_id,
            mode: SessionMode::Otp,
            prefilter: json!({}),
        },
    )
    .await;

    let incoming = recv_msg(&mut host).await;
    assert_eq!(incoming.signal_seq(), Some(11));
    assert_eq!(incoming.session_id(), Some("sess-seq"));
}

/// Shared setup: online host + anonymous viewer after hello_ok.
async fn connect_host_viewer(addr: SocketAddr, access: &str) -> (WsStream, WsStream) {
    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(access)).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));
    let mut viewer = connect_ws(addr).await;
    send_msg(&mut viewer, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));
    (host, viewer)
}

async fn intent(viewer: &mut WsStream, session_id: &str, host_public_id: &str, signal_seq: u64) {
    send_msg(
        viewer,
        &SignalMessage::SessionIntent {
            session_id: session_id.into(),
            signal_seq,
            host_public_id: host_public_id.into(),
            mode: SessionMode::Otp,
            prefilter: json!({}),
        },
    )
    .await;
}

#[tokio::test]
async fn host_disconnect_releases_busy_and_notifies_viewer() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-disc-host", &host_public_id, 1).await;

    // Drain session_incoming on host before dropping it.
    let mut host = host;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_some());

    drop(host);

    match recv_msg(&mut viewer).await {
        SignalMessage::SessionEnd {
            reason, session_id, ..
        } => {
            assert_eq!(session_id, "sess-disc-host");
            assert_eq!(reason, "peer_disconnected");
        }
        other => panic!("expected session_end, got {other:?}"),
    }
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_none());
}

#[tokio::test]
async fn viewer_disconnect_releases_busy_and_notifies_host() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-disc-viewer", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));

    drop(viewer);

    match recv_msg(&mut host).await {
        SignalMessage::SessionEnd { reason, .. } => {
            assert_eq!(reason, "peer_disconnected");
        }
        other => panic!("expected session_end, got {other:?}"),
    }
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_none());
}

#[tokio::test]
async fn busy_lock_while_active_rejects_second_intent() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-active-busy", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    send_msg(
        &mut host,
        &SignalMessage::SessionAccept {
            session_id: "sess-active-busy".into(),
            signal_seq: 3,
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::SessionAccept { .. }
    ));
    assert_eq!(
        sessions.session_state("sess-active-busy").await,
        Some(remotelink_server::SessionState::Active)
    );

    let mut v2 = connect_ws(addr).await;
    send_msg(&mut v2, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut v2).await,
        SignalMessage::HelloOk { .. }
    ));
    intent(&mut v2, "sess-active-busy-2", &host_public_id, 1).await;
    match recv_msg(&mut v2).await {
        SignalMessage::SessionReject { reason, .. } => {
            assert_eq!(reason, RejectReason::Busy);
        }
        other => panic!("expected busy while active, got {other:?}"),
    }
}

#[tokio::test]
async fn session_end_releases_busy() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-end-1", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    send_msg(
        &mut host,
        &SignalMessage::SessionAccept {
            session_id: "sess-end-1".into(),
            signal_seq: 3,
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::SessionAccept { .. }
    ));

    send_msg(
        &mut host,
        &SignalMessage::SessionEnd {
            session_id: "sess-end-1".into(),
            signal_seq: 4,
            reason: "host_hangup".into(),
        },
    )
    .await;

    match recv_msg(&mut viewer).await {
        SignalMessage::SessionEnd { reason, .. } => assert_eq!(reason, "host_hangup"),
        other => panic!("expected session_end, got {other:?}"),
    }
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_none());
}

#[tokio::test]
async fn host_hello_rejects_expired_access_token() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let public_id = DevicePublicId::generate().into_string();
    let device = repo
        .create_device(NewDevice {
            public_id,
            display_name: None,
            public_key: vec![9; 32],
            protocol_version_last: Some(1),
        })
        .await
        .unwrap();
    let now = Utc::now();
    let access = "rl_at_expired_for_ws_hello_0000000001";
    repo.insert_credential(NewCredential {
        device_id: device.id,
        token_hash: hash_token(access),
        refresh_token_hash: hash_token("rl_rt_still_ok"),
        access_expires_at: now - chrono::Duration::minutes(1),
        expires_at: now + chrono::Duration::days(7),
    })
    .await
    .unwrap();

    let state = AppState::new(repo);
    let addr = spawn_server(state).await;
    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(access)).await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("expired") || message.contains("unauthorized"));
        }
        other => panic!("expected unauthorized, got {other:?}"),
    }
}

#[tokio::test]
async fn host_hello_rejects_disabled_device() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let (public_id, access) = register_host(&repo).await;
    repo.set_status(&public_id, DeviceStatus::Disabled).unwrap();

    let state = AppState::new(repo);
    let addr = spawn_server(state).await;
    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "unauthorized"),
        other => panic!("expected unauthorized, got {other:?}"),
    }
}

#[tokio::test]
async fn role_reverse_viewer_cannot_accept_host_cannot_intent() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;

    // Host must not send session_intent.
    intent(&mut host, "sess-role", &host_public_id, 1).await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("viewers"));
        }
        other => panic!("expected unauthorized for host intent, got {other:?}"),
    }

    // Viewer must not send session_accept.
    send_msg(
        &mut viewer,
        &SignalMessage::SessionAccept {
            session_id: "nope".into(),
            signal_seq: 1,
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("hosts"));
        }
        other => panic!("expected unauthorized for viewer accept, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_session_id_conflicts() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions);
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-dup", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));

    // Same session_id from another viewer while first is still pending.
    // Busy rejects first when same host; use reject path by ending first?
    // Actually host is busy so we get busy not conflict. Force conflict by
    // same session_id on a free host — register second host.
    let (host2_id, access2) = register_host(&repo).await;
    let mut host2 = connect_ws(addr).await;
    send_msg(&mut host2, &hello_host(&access2)).await;
    assert!(matches!(
        recv_msg(&mut host2).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut v2 = connect_ws(addr).await;
    send_msg(&mut v2, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut v2).await,
        SignalMessage::HelloOk { .. }
    ));
    // Reuse session_id that still exists on first host's pending session.
    intent(&mut v2, "sess-dup", &host2_id, 1).await;
    match recv_msg(&mut v2).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "conflict"),
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn stale_signal_seq_rejected_on_accept() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-stale-seq", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    // next_signal_seq is 3 after intent(1) → incoming(2)
    assert_eq!(sessions.next_signal_seq("sess-stale-seq").await, Some(3));

    send_msg(
        &mut host,
        &SignalMessage::SessionAccept {
            session_id: "sess-stale-seq".into(),
            signal_seq: 0, // stale
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "stale_signal_seq"),
        other => panic!("expected stale_signal_seq, got {other:?}"),
    }
    // Still pending — not advanced.
    assert_eq!(
        sessions.session_state("sess-stale-seq").await,
        Some(remotelink_server::SessionState::Pending)
    );
}

#[tokio::test]
async fn pending_session_ttl_releases_busy() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-ttl", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_some());

    assert!(sessions.force_expire_session("sess-ttl").await);

    // Both peers should see session_end reason=session_ttl (order nondeterministic
    // if both read; check busy released and at least one peer notified).
    // force_expire reaps under the lock and sends to both; drain both.
    let mut saw_ttl = false;
    for peer in [&mut host, &mut viewer] {
        if let Ok(SignalMessage::SessionEnd { reason, .. }) =
            tokio::time::timeout(Duration::from_millis(500), async {
                loop {
                    let frame = peer.next().await.expect("ws").expect("err");
                    match frame {
                        Message::Text(t) => return decode_message(t.as_str()).unwrap(),
                        Message::Ping(p) => {
                            peer.send(Message::Pong(p)).await.ok();
                        }
                        _ => {}
                    }
                }
            })
            .await
        {
            assert_eq!(reason, "session_ttl");
            saw_ttl = true;
        }
    }
    assert!(saw_ttl, "expected at least one session_ttl end");
    assert!(sessions
        .busy_session_for_host(&host_public_id)
        .await
        .is_none());

    // Host free for a new intent.
    intent(&mut viewer, "sess-after-ttl", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming {
            session_id: ref sid,
            ..
        } if sid == "sess-after-ttl"
    ));
}

#[tokio::test]
async fn host_reconnect_rebinds_pending_session() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host_a, mut viewer) = connect_host_viewer(addr, &access).await;
    intent(&mut viewer, "sess-rebind", &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host_a).await,
        SignalMessage::SessionIncoming { .. }
    ));

    // Second host connection with same token replaces presence.
    let mut host_b = connect_ws(addr).await;
    send_msg(&mut host_b, &hello_host(&access)).await;
    assert!(matches!(
        recv_msg(&mut host_b).await,
        SignalMessage::HelloOk { .. }
    ));

    // Old socket should see connection_replaced (best-effort).
    match recv_msg(&mut host_a).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "connection_replaced"),
        other => panic!("expected connection_replaced, got {other:?}"),
    }

    // New socket can accept the transferred pending session.
    send_msg(
        &mut host_b,
        &SignalMessage::SessionAccept {
            session_id: "sess-rebind".into(),
            signal_seq: 3,
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::SessionAccept { session_id, .. } => {
            assert_eq!(session_id, "sess-rebind");
        }
        other => panic!("expected accept after rebind, got {other:?}"),
    }
    assert_eq!(
        sessions.session_state("sess-rebind").await,
        Some(remotelink_server::SessionState::Active)
    );
}

/// Accept a pending session and return the next usable client signal_seq (4 after intent1/incoming2/accept3).
async fn accept_session(
    host: &mut WsStream,
    viewer: &mut WsStream,
    session_id: &str,
    accept_seq: u64,
) {
    send_msg(
        host,
        &SignalMessage::SessionAccept {
            session_id: session_id.into(),
            signal_seq: accept_seq,
        },
    )
    .await;
    match recv_msg(viewer).await {
        SignalMessage::SessionAccept {
            session_id: sid,
            signal_seq,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, accept_seq);
        }
        other => panic!("expected session_accept, got {other:?}"),
    }
}

#[tokio::test]
async fn sdp_and_ice_relay_after_accept() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    let session_id = "sess-sdp-ice";
    intent(&mut viewer, session_id, &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    accept_session(&mut host, &mut viewer, session_id, 3).await;
    assert_eq!(
        sessions.session_state(session_id).await,
        Some(remotelink_server::SessionState::Active)
    );
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));

    let offer_sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n";
    let fingerprint_sig = "sig-host-fingerprint";
    send_msg(
        &mut host,
        &SignalMessage::SessionOffer {
            session_id: session_id.into(),
            signal_seq: 4,
            sdp: offer_sdp.into(),
            fingerprint_sig: fingerprint_sig.into(),
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::SessionOffer {
            session_id: sid,
            signal_seq,
            sdp,
            fingerprint_sig: sig,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 4);
            assert_eq!(sdp, offer_sdp);
            assert_eq!(sig, fingerprint_sig);
        }
        other => panic!("expected session_offer, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(5));

    let answer_sdp = "v=0\r\no=- 1 0 IN IP4 0.0.0.0\r\ns=-\r\n";
    send_msg(
        &mut viewer,
        &SignalMessage::SessionAnswer {
            session_id: session_id.into(),
            signal_seq: 5,
            sdp: answer_sdp.into(),
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::SessionAnswer {
            session_id: sid,
            signal_seq,
            sdp,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 5);
            assert_eq!(sdp, answer_sdp);
        }
        other => panic!("expected session_answer, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(6));

    // ICE either direction, payloads forwarded verbatim.
    let host_cand = IceCandidate {
        candidate: "candidate:1 1 UDP 2122252543 192.0.2.1 54321 typ host".into(),
        sdp_mid: Some("0".into()),
        sdp_m_line_index: Some(0),
        username_fragment: None,
    };
    send_msg(
        &mut host,
        &SignalMessage::IceCandidate {
            session_id: session_id.into(),
            signal_seq: 6,
            candidate: host_cand.clone(),
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::IceCandidate {
            session_id: sid,
            signal_seq,
            candidate,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 6);
            assert_eq!(candidate, host_cand);
        }
        other => panic!("expected ice_candidate host→viewer, got {other:?}"),
    }

    let viewer_cand = IceCandidate {
        candidate: "candidate:2 1 UDP 1686052607 198.51.100.1 9 typ srflx".into(),
        sdp_mid: Some("0".into()),
        sdp_m_line_index: Some(0),
        username_fragment: Some("ufrag1".into()),
    };
    send_msg(
        &mut viewer,
        &SignalMessage::IceCandidate {
            session_id: session_id.into(),
            signal_seq: 7,
            candidate: viewer_cand.clone(),
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::IceCandidate {
            session_id: sid,
            signal_seq,
            candidate,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 7);
            assert_eq!(candidate, viewer_cand);
        }
        other => panic!("expected ice_candidate viewer→host, got {other:?}"),
    }

    // Optional media control messages also forward either direction.
    send_msg(
        &mut host,
        &SignalMessage::MediaRestart {
            session_id: session_id.into(),
            signal_seq: 8,
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::MediaRestart {
            session_id: sid,
            signal_seq,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 8);
        }
        other => panic!("expected media_restart, got {other:?}"),
    }

    send_msg(
        &mut viewer,
        &SignalMessage::Renegotiate {
            session_id: session_id.into(),
            signal_seq: 9,
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Renegotiate {
            session_id: sid,
            signal_seq,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(signal_seq, 9);
        }
        other => panic!("expected renegotiate, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(10));
}

#[tokio::test]
async fn offer_answer_direction_and_active_only() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    let session_id = "sess-dir";
    intent(&mut viewer, session_id, &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));

    // Pending: offer must not relay.
    send_msg(
        &mut host,
        &SignalMessage::SessionOffer {
            session_id: session_id.into(),
            signal_seq: 3,
            sdp: "v=0".into(),
            fingerprint_sig: "sig".into(),
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "invalid_state"),
        other => panic!("expected invalid_state on pending offer, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(3));

    accept_session(&mut host, &mut viewer, session_id, 3).await;

    // Viewer must not send offer.
    send_msg(
        &mut viewer,
        &SignalMessage::SessionOffer {
            session_id: session_id.into(),
            signal_seq: 4,
            sdp: "v=0".into(),
            fingerprint_sig: "sig".into(),
        },
    )
    .await;
    match recv_msg(&mut viewer).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("host"));
        }
        other => panic!("expected unauthorized for viewer offer, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));

    // Host must not send answer.
    send_msg(
        &mut host,
        &SignalMessage::SessionAnswer {
            session_id: session_id.into(),
            signal_seq: 4,
            sdp: "v=0".into(),
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("viewer"));
        }
        other => panic!("expected unauthorized for host answer, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));
}

#[tokio::test]
async fn relay_rejects_stale_signal_seq_and_non_party() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    let session_id = "sess-relay-authz";
    intent(&mut viewer, session_id, &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    accept_session(&mut host, &mut viewer, session_id, 3).await;

    // Stale seq after accept (next is 4).
    send_msg(
        &mut host,
        &SignalMessage::SessionOffer {
            session_id: session_id.into(),
            signal_seq: 2,
            sdp: "v=0".into(),
            fingerprint_sig: "sig".into(),
        },
    )
    .await;
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "stale_signal_seq"),
        other => panic!("expected stale_signal_seq, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));

    // Third peer is not a party.
    let mut outsider = connect_ws(addr).await;
    send_msg(&mut outsider, &hello_viewer("")).await;
    assert!(matches!(
        recv_msg(&mut outsider).await,
        SignalMessage::HelloOk { .. }
    ));
    send_msg(
        &mut outsider,
        &SignalMessage::IceCandidate {
            session_id: session_id.into(),
            signal_seq: 4,
            candidate: IceCandidate {
                candidate: "candidate:x".into(),
                sdp_mid: None,
                sdp_m_line_index: None,
                username_fragment: None,
            },
        },
    )
    .await;
    match recv_msg(&mut outsider).await {
        SignalMessage::Error { code, .. } => assert_eq!(code, "unauthorized"),
        other => panic!("expected unauthorized for non-party, got {other:?}"),
    }
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));
}

#[tokio::test]
async fn oversized_sdp_rejected_before_relay() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let (mut host, mut viewer) = connect_host_viewer(addr, &access).await;
    let session_id = "sess-sdp-limit";
    intent(&mut viewer, session_id, &host_public_id, 1).await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::SessionIncoming { .. }
    ));
    accept_session(&mut host, &mut viewer, session_id, 3).await;

    // encode_message does not re-check limits; craft JSON that decode will reject.
    let huge = "x".repeat(MAX_SDP_BYTES + 1);
    let raw = format!(
        r#"{{"type":"session_offer","session_id":"{session_id}","signal_seq":4,"sdp":"{huge}","fingerprint_sig":"sig"}}"#
    );
    host.send(Message::Text(raw.into())).await.unwrap();
    match recv_msg(&mut host).await {
        SignalMessage::Error { code, message } => {
            assert_eq!(code, "protocol_error");
            assert!(
                message.contains("too large")
                    || message.contains("sdp")
                    || message.contains("invalid"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected protocol_error for oversized sdp, got {other:?}"),
    }
    // Counter must not advance when decode fails.
    assert_eq!(sessions.next_signal_seq(session_id).await, Some(4));
}

#[tokio::test]
async fn hello_ok_advertises_sdp_relay() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo.clone());
    let (_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(&mut host, &hello_host(&access)).await;
    match recv_msg(&mut host).await {
        SignalMessage::HelloOk { feature_flags, .. } => {
            assert_eq!(feature_flags["sdp_relay"], true);
        }
        other => panic!("expected hello_ok, got {other:?}"),
    }
}
