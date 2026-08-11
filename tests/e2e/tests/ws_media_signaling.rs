//! End-to-end: WSS session lifecycle + SDP/ICE relay + SessionManager media.
//!
//! Proves the product path:
//! 1. Host and viewer `hello` on `/v1/ws`
//! 2. `session_intent` → `session_incoming` → `session_accept`
//! 3. Host `session_offer` / viewer `session_answer` / ICE relayed by the server
//! 4. Host [`SessionManager`] pumps synthetic A/V; viewer [`ViewerSession`] records it
//!
//! Media uses an in-process [`MockPeerPair`] (CI-safe). Signaling is real WebSocket
//! on `127.0.0.1:0`. Live/webrtc multi-process demos use the same WSS messages.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use remotelink_auth::DevicePublicId;
use remotelink_host::{
    parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager,
};
use remotelink_net::{ConnectionState, MockPeerPair, SessionDescription};
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SignalMessage, PROTOCOL_VERSION,
};
use remotelink_server::credentials::{mint_tokens, new_credential_from_issued};
use remotelink_server::{
    router, AppState, DeviceRepository, MemoryDeviceRepo, NewDevice, SessionRegistry, SessionState,
};
use remotelink_viewer_core::{ConnectRequest, ViewerSession};
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
            display_name: Some("e2e-media-host".into()),
            public_key: vec![21; 32],
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
    tokio::time::timeout(Duration::from_secs(5), async {
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
async fn ws_sdp_relay_session_manager_media() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let sessions = Arc::new(SessionRegistry::new());
    let state = AppState::with_sessions(repo.clone(), sessions.clone());
    let (host_public_id, access) = register_host(&repo).await;
    let addr = spawn_server(state).await;

    // --- Signaling: hello ---
    let mut host_ws = connect_ws(addr).await;
    send_msg(
        &mut host_ws,
        &SignalMessage::Hello {
            role: Role::Host,
            protocol_version: PROTOCOL_VERSION,
            auth: HelloAuth {
                device_token: access,
            },
        },
    )
    .await;
    match recv_msg(&mut host_ws).await {
        SignalMessage::HelloOk { feature_flags, .. } => {
            assert_eq!(feature_flags["sdp_relay"], true);
        }
        other => panic!("host hello_ok: {other:?}"),
    }

    let mut viewer_ws = connect_ws(addr).await;
    send_msg(
        &mut viewer_ws,
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
        recv_msg(&mut viewer_ws).await,
        SignalMessage::HelloOk { .. }
    ));

    let session_id = "e2e-ws-media-1";
    let intent = ConnectRequest::otp(&host_public_id, "123456")
        .session_intent_message(session_id, 1)
        .expect("session_intent");
    send_msg(&mut viewer_ws, &intent).await;

    match recv_msg(&mut host_ws).await {
        SignalMessage::SessionIncoming {
            session_id: sid, ..
        } => assert_eq!(sid, session_id),
        other => panic!("session_incoming: {other:?}"),
    }

    send_msg(
        &mut host_ws,
        &SignalMessage::SessionAccept {
            session_id: session_id.into(),
            signal_seq: 3,
        },
    )
    .await;
    assert!(matches!(
        recv_msg(&mut viewer_ws).await,
        SignalMessage::SessionAccept { .. }
    ));
    assert_eq!(
        sessions.session_state(session_id).await,
        Some(SessionState::Active)
    );

    // --- Media plane: mock pair + SessionManager / ViewerSession ---
    let pair = MockPeerPair::new();
    // Viewer transport callbacks go through ViewerSession; host side on SessionManager.
    let MockPeerPair { peer_a, peer_b } = pair;

    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.attach(session_id);

    let mut viewer = ViewerSession::new();
    viewer
        .begin_connect(&ConnectRequest::otp(&host_public_id, "123456"))
        .expect("begin_connect");
    // Align viewer session id with WSS session for bind bookkeeping.
    // begin_connect sets its own stub id; attach transport after.
    viewer.attach_transport(Box::new(peer_b));

    host.start_media().expect("start_media");
    let outbound = host.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .expect("session_offer from host");
    let offer_payload = parse_sdp_payload(&offer_sig.payload).expect("parse offer");

    // Relay offer over WSS (seq 4 after accept@3).
    send_msg(
        &mut host_ws,
        &SignalMessage::SessionOffer {
            session_id: session_id.into(),
            signal_seq: 4,
            sdp: offer_payload.sdp.clone(),
            fingerprint_sig: offer_payload.fingerprint_sig.clone().unwrap_or_default(),
        },
    )
    .await;

    let remote_offer = match recv_msg(&mut viewer_ws).await {
        SignalMessage::SessionOffer { sdp, fingerprint_sig, .. } => (sdp, fingerprint_sig),
        other => panic!("viewer expected offer, got {other:?}"),
    };

    let answer = viewer
        .accept_offer_with_sig(
            SessionDescription::offer(remote_offer.0),
            if remote_offer.1.is_empty() {
                None
            } else {
                Some(remote_offer.1.as_str())
            },
        )
        .expect("accept_offer");

    send_msg(
        &mut viewer_ws,
        &SignalMessage::SessionAnswer {
            session_id: session_id.into(),
            signal_seq: 5,
            sdp: answer.sdp.clone(),
        },
    )
    .await;

    match recv_msg(&mut host_ws).await {
        SignalMessage::SessionAnswer { sdp, .. } => {
            host.apply_signal(
                signal_kind::SESSION_ANSWER,
                &serde_json::to_string(&SdpPayload {
                    sdp,
                    fingerprint_sig: None,
                })
                .unwrap(),
            )
            .expect("apply answer");
        }
        other => panic!("host expected answer, got {other:?}"),
    }

    // Host ICE → WSS → viewer
    let mut seq = 6u64;
    for sig in host.take_outbound_signals() {
        if sig.kind == signal_kind::ICE_CANDIDATE {
            let c = parse_ice_payload(&sig.payload).expect("host ice");
            send_msg(
                &mut host_ws,
                &SignalMessage::IceCandidate {
                    session_id: session_id.into(),
                    signal_seq: seq,
                    candidate: c,
                },
            )
            .await;
            match recv_msg(&mut viewer_ws).await {
                SignalMessage::IceCandidate { candidate, .. } => {
                    viewer.add_remote_ice(candidate).expect("viewer ice");
                }
                other => panic!("viewer expected ice, got {other:?}"),
            }
            seq += 1;
        }
    }
    // Offer-time ICE that was drained with the offer
    for sig in outbound
        .iter()
        .filter(|s| s.kind == signal_kind::ICE_CANDIDATE)
    {
        let c = parse_ice_payload(&sig.payload).expect("offer ice");
        send_msg(
            &mut host_ws,
            &SignalMessage::IceCandidate {
                session_id: session_id.into(),
                signal_seq: seq,
                candidate: c,
            },
        )
        .await;
        match recv_msg(&mut viewer_ws).await {
            SignalMessage::IceCandidate { candidate, .. } => {
                viewer.add_remote_ice(candidate).expect("viewer offer ice");
            }
            other => panic!("viewer expected offer ice, got {other:?}"),
        }
        seq += 1;
    }

    // Viewer ICE → WSS → host
    for ice in viewer.take_pending_local_ice() {
        send_msg(
            &mut viewer_ws,
            &SignalMessage::IceCandidate {
                session_id: session_id.into(),
                signal_seq: seq,
                candidate: ice,
            },
        )
        .await;
        match recv_msg(&mut host_ws).await {
            SignalMessage::IceCandidate { candidate, .. } => {
                host.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).unwrap(),
                )
                .expect("host apply viewer ice");
            }
            other => panic!("host expected ice, got {other:?}"),
        }
        seq += 1;
    }

    viewer.poll().expect("viewer poll");
    assert_eq!(host.connection_state(), ConnectionState::Connected);
    assert_eq!(
        viewer.transport_state(),
        Some(ConnectionState::Connected)
    );

    let pump = host.pump_media(3).expect("pump");
    assert_eq!(pump.video_sent, 3);
    assert!(!pump.skipped_not_connected);

    // Deliver mock media into viewer session.
    for _ in 0..20 {
        viewer.poll().expect("viewer poll media");
        if viewer.recorded_video_nalus().len() >= 3
            && !viewer.recorded_audio_packets().is_empty()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        viewer.recorded_video_nalus().len() >= 3,
        "viewer video nalu count={}",
        viewer.recorded_video_nalus().len()
    );
    assert!(
        !viewer.recorded_audio_packets().is_empty(),
        "viewer should receive audio"
    );

    let _ = seq;
}
