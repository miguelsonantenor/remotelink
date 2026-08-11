//! Multi-process-style lab: real server + host/viewer WSS + **live TCP** media.
//!
//! Uses library entry points so CI does not need three terminals. Same path as:
//!
//! ```text
//! remotelink-server
//! remotelink-host --role=ws --server=http://127.0.0.1:PORT --transport=live
//! remotelink-viewer --ws-connect --server=… --host PUBLIC_ID --transport=live
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use remotelink_auth::generate_device_keypair;
use remotelink_host::{run_ws_host, ExistingHostCreds, WsHostConfig};
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

/// Viewer half of the CLI path (mirrors `apps/viewer/src/ws_connect.rs`).
async fn run_ws_viewer_live(
    server: &str,
    host_public_id: &str,
) -> Result<String, String> {
    let transport_cfg = TransportConfig {
        mode: TransportMode::Live,
    };
    let ws_url = http_to_ws_url(server).map_err(|e| e.to_string())?;
    let mut sig = SignalingClient::connect(&ws_url)
        .await
        .map_err(|e| e.to_string())?;
    sig.hello_viewer_anonymous()
        .await
        .map_err(|e| e.to_string())?;

    let session_id = format!("e2e-live-{}", std::process::id());
    let intent_seq = sig.take_seq();
    let req = ConnectRequest::otp(host_public_id, "123456");
    let intent = req
        .session_intent_message(&session_id, intent_seq)
        .map_err(|e| e.to_string())?;
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
    sig.send(&intent).await.map_err(|e| e.to_string())?;

    let accept = sig
        .recv_until(Duration::from_secs(30), |m| {
            matches!(
                m,
                SignalMessage::SessionAccept { .. } | SignalMessage::SessionReject { .. }
            )
        })
        .await
        .map_err(|e| e.to_string())?;
    match accept {
        SignalMessage::SessionAccept { .. } => {}
        SignalMessage::SessionReject { reason, .. } => {
            return Err(format!("rejected: {reason:?}"));
        }
        _ => {}
    }

    let answerer = create_peer_transport_with_config(PeerRole::Answerer, &transport_cfg)
        .map_err(|e| e.to_string())?;
    let mut viewer = ViewerSession::new();
    viewer.begin_connect(&req).map_err(|e| e.to_string())?;
    viewer.attach_transport(answerer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
            Err(e) => return Err(e.to_string()),
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
                    .map_err(|e| e.to_string())?;
                let ans_seq = sig.take_seq();
                sig.send(&SignalMessage::SessionAnswer {
                    session_id: session_id.clone(),
                    signal_seq: ans_seq,
                    sdp: answer.sdp,
                })
                .await
                .map_err(|e| e.to_string())?;
                got_offer = true;
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                viewer.add_remote_ice(candidate).map_err(|e| e.to_string())?;
                let _ = viewer.poll();
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
            _ => {}
        }
        let _ = viewer.poll();
        if got_offer && viewer.transport_state() == Some(ConnectionState::Connected) {
            break;
        }
    }
    if !got_offer {
        return Err("no offer".into());
    }

    let media_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < media_deadline {
        let _ = viewer.poll();
        if !viewer.recorded_video_nalus().is_empty()
            && !viewer.recorded_audio_packets().is_empty()
        {
            break;
        }
        if let Ok(SignalMessage::IceCandidate { candidate, .. }) =
            sig.recv_timeout(Duration::from_millis(30)).await
        {
            let _ = viewer.add_remote_ice(candidate);
        } else {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    let v = viewer.recorded_video_nalus().len();
    let a = viewer.recorded_audio_packets().len();
    if v == 0 {
        return Err(format!("no video (audio={a})"));
    }
    Ok(format!(
        "ws-viewer ok host={host_public_id} video_rx={v} audio_rx={a}"
    ))
}

#[tokio::test]
async fn ws_host_viewer_live_tcp_media() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo);
    let addr = spawn_server(state).await;
    let server = format!("http://{addr}");

    let (_sk, vk) = generate_device_keypair();
    let pk = vk.to_bytes();
    let reg = register_device(&server, &pk, Some("e2e-live-host"))
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
        }),
    };

    let host_handle = tokio::spawn(async move { run_ws_host(host_cfg).await });
    tokio::time::sleep(Duration::from_millis(250)).await;

    let viewer_summary = run_ws_viewer_live(&server, &reg.public_id)
        .await
        .expect("viewer ws session");
    let host_summary = host_handle
        .await
        .expect("host join")
        .expect("host ws session");

    assert!(
        host_summary.contains("video_tx="),
        "host summary: {host_summary}"
    );
    assert!(
        viewer_summary.contains("video_rx="),
        "viewer summary: {viewer_summary}"
    );
    println!("host: {host_summary}");
    println!("viewer: {viewer_summary}");
}
