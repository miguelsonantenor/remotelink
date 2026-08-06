//! Light in-process server smoke: hello + session_intent + accept.
//!
//! Binds `127.0.0.1:0` only — no external network. Optional PR 15 coverage
//! that server APIs still compile and wire with host/viewer session pieces.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_auth::DevicePublicId;
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SessionMode, SignalMessage, PROTOCOL_VERSION,
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
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    tokio::task::yield_now().await;
    addr
}

async fn register_host(repo: &MemoryDeviceRepo) -> (String, String) {
    let public_id = DevicePublicId::generate().into_string();
    let device = repo
        .create_device(NewDevice {
            public_id: public_id.clone(),
            display_name: Some("e2e-ws-host".into()),
            public_key: vec![15; 32],
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

#[tokio::test]
async fn session_intent_accept_smoke() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    let mut host = connect_ws(addr).await;
    send_msg(
        &mut host,
        &SignalMessage::Hello {
            role: Role::Host,
            protocol_version: PROTOCOL_VERSION,
            auth: HelloAuth {
                device_token: access,
            },
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut host).await,
        SignalMessage::HelloOk { .. }
    ));

    let mut viewer = connect_ws(addr).await;
    send_msg(
        &mut viewer,
        &SignalMessage::Hello {
            role: Role::Viewer,
            protocol_version: PROTOCOL_VERSION,
            auth: HelloAuth {
                device_token: String::new(),
            },
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut viewer).await,
        SignalMessage::HelloOk { .. }
    ));

    // Viewer-core session_intent builder still matches protocol wire authority.
    let intent_req = remotelink_viewer_core::ConnectRequest::otp(&host_public_id, "123456");
    let session_id = "e2e-ws-sess-1";
    let intent = intent_req
        .session_intent_message(session_id, 1)
        .expect("session_intent_message");
    // Override mode/prefilter already correct for OTP; send as-built or explicit.
    let _ = intent;
    send_msg(
        &mut viewer,
        &SignalMessage::SessionIntent {
            session_id: session_id.into(),
            signal_seq: 1,
            host_public_id: host_public_id.clone(),
            mode: SessionMode::Otp,
            prefilter: json!({ "otp": "123456" }),
        },
    )
    .await;

    match recv_msg(&mut host).await {
        SignalMessage::SessionIncoming {
            session_id: sid, ..
        } => assert_eq!(sid, session_id),
        other => panic!("expected session_incoming, got {other:?}"),
    }

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
            session_id: sid, ..
        } => assert_eq!(sid, session_id),
        other => panic!("expected session_accept, got {other:?}"),
    }

    assert_eq!(
        sessions.session_state(session_id).await,
        Some(remotelink_server::SessionState::Active)
    );
}
