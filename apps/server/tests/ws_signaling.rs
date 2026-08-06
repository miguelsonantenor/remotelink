//! Integration tests for `/v1/ws` hello + session_intent + accept/reject.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_auth::DevicePublicId;
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, RejectReason, Role, SessionMode, SignalMessage,
    PROTOCOL_VERSION,
};
use remotelink_server::credentials::{mint_tokens, new_credential_from_issued};
use remotelink_server::{
    router, AppState, DeviceRepository, MemoryDeviceRepo, NewDevice, SessionRegistry,
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
