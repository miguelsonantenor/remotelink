//! KD5 split: WSS service + agent control IPC + live media (one process, two threads).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use remotelink_auth::generate_device_keypair;
use remotelink_host::{
    listen_control, run_ws_host, serve_agent_connection, ControlEndpoint, ExistingHostCreds,
    WsHostConfig,
};
use remotelink_net::{
    create_peer_transport_with_config, ConnectionState, PeerRole, SessionDescription,
    TransportConfig, TransportMode,
};
use remotelink_protocol::{SessionMode, SignalMessage};
use remotelink_server::{router, AppState, MemoryDeviceRepo};
use remotelink_signaling::{http_to_ws_url, register_device, SignalingClient};
use remotelink_viewer_core::{ConnectRequest, ViewerSession};
use tokio::net::TcpListener;

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

#[tokio::test]
async fn wss_service_drives_agent_over_ipc_live() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo);
    let addr = spawn_server(state).await;
    let server = format!("http://{addr}");

    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let agent_thread = std::thread::spawn(move || {
        let listener = listen_control(ControlEndpoint::tcp_localhost(0)).unwrap();
        let port = listener.tcp_port().unwrap();
        port_tx.send(port).unwrap();
        let mut stream = listener.accept().unwrap();
        let mut agent =
            remotelink_host::AgentSession::from_mode(TransportMode::Live).expect("live agent");
        let _ = serve_agent_connection(&mut stream, &mut agent, TransportMode::Live);
    });

    let control_port = port_rx.recv().expect("agent control port");

    let (_sk, vk) = generate_device_keypair();
    let reg = register_device(&server, &vk.to_bytes(), Some("ws-agent-ipc"))
        .await
        .expect("register");

    let host_cfg = WsHostConfig {
        server: server.clone(),
        display_name: "e2e".into(),
        transport: TransportMode::Live,
        video_frames: 3,
        wait_incoming: Duration::from_secs(30),
        existing: Some(ExistingHostCreds {
            public_id: reg.public_id.clone(),
            access_token: reg.access_token.clone(),
            refresh_token: Some(reg.refresh_token.clone()),
        }),
        max_sessions: 1,
        reconnect: false,
        reconnect_backoff: Duration::from_secs(1),
        creds_path: std::env::temp_dir().join("rl-ws-agent-ipc-creds.json"),
        load_creds: false,
        save_creds: false,
        mint_otp: false,
        agent_control: Some(ControlEndpoint::tcp_localhost(control_port)),
    };

    let host_task = tokio::spawn(async move { run_ws_host(host_cfg).await });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Viewer: WSS + live answerer
    let transport_cfg = TransportConfig {
        mode: TransportMode::Live,
    };
    let ws_url = http_to_ws_url(&server).unwrap();
    let mut sig = SignalingClient::connect(&ws_url).await.unwrap();
    sig.hello_viewer_anonymous().await.unwrap();

    let session_id = format!("ws-agent-ipc-{}", std::process::id());
    let intent_seq = sig.take_seq();
    let req = ConnectRequest::otp(&reg.public_id, "123456");
    let intent = req.session_intent_message(&session_id, intent_seq).unwrap();
    let intent = match intent {
        SignalMessage::SessionIntent {
            session_id,
            signal_seq,
            host_public_id,
            prefilter,
            ..
        } => SignalMessage::SessionIntent {
            session_id,
            signal_seq,
            host_public_id,
            mode: SessionMode::Otp,
            prefilter,
        },
        other => other,
    };
    sig.send(&intent).await.unwrap();

    let accept = sig
        .recv_until(Duration::from_secs(30), |m| {
            matches!(
                m,
                SignalMessage::SessionAccept { .. } | SignalMessage::SessionReject { .. }
            )
        })
        .await
        .expect("accept");
    assert!(matches!(accept, SignalMessage::SessionAccept { .. }));

    let answerer = create_peer_transport_with_config(PeerRole::Answerer, &transport_cfg).unwrap();
    let mut viewer = ViewerSession::new();
    viewer.begin_connect(&req).unwrap();
    viewer.attach_transport(answerer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    let mut got_offer = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match sig
            .recv_timeout(remaining.min(Duration::from_millis(200)))
            .await
        {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {
                let _ = viewer.poll();
                continue;
            }
            Err(e) => panic!("viewer recv: {e}"),
        };
        match msg {
            SignalMessage::SessionOffer {
                sdp,
                fingerprint_sig,
                ..
            } => {
                let answer = viewer
                    .accept_offer_with_sig(
                        SessionDescription::offer(sdp),
                        if fingerprint_sig.is_empty() {
                            None
                        } else {
                            Some(fingerprint_sig.as_str())
                        },
                    )
                    .expect("accept_offer");
                let ans_seq = sig.take_seq();
                sig.send(&SignalMessage::SessionAnswer {
                    session_id: session_id.clone(),
                    signal_seq: ans_seq,
                    sdp: answer.sdp,
                })
                .await
                .unwrap();
                got_offer = true;
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .unwrap();
                }
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                viewer.add_remote_ice(candidate).unwrap();
                let _ = viewer.poll();
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .unwrap();
                }
            }
            _ => {}
        }
        let _ = viewer.poll();
        if got_offer && viewer.transport_state() == Some(ConnectionState::Connected) {
            break;
        }
    }
    assert!(got_offer, "expected offer from agent via service WSS");

    let media_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < media_deadline {
        let _ = viewer.poll();
        if !viewer.recorded_video_nalus().is_empty() {
            break;
        }
        if let Ok(SignalMessage::IceCandidate { candidate, .. }) =
            sig.recv_timeout(Duration::from_millis(40)).await
        {
            let _ = viewer.add_remote_ice(candidate);
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    let v = viewer.recorded_video_nalus().len();
    assert!(v > 0, "expected video from agent live path, got 0");

    let host_summary = host_task.await.expect("host join").expect("host ok");
    assert!(
        host_summary.contains("media=agent"),
        "host_summary={host_summary}"
    );
    println!("host: {host_summary}");
    println!("viewer: video_rx={v}");

    let _ = agent_thread.join();
}
