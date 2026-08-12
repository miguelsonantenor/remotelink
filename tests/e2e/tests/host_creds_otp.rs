//! Credential file persistence + Mode A OTP mint against a live server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use remotelink_auth::{generate_device_keypair, mint_otp};
use remotelink_host::DEFAULT_HOST_OTP_PEPPER;
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SessionMode, SignalMessage, PROTOCOL_VERSION,
};
use remotelink_server::{router, AppState, MemoryDeviceRepo};
use remotelink_signaling::{
    http_to_ws_url, post_otp_hash, register_device, HostCredentialFile, SignalingClient,
};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

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
async fn register_save_load_and_otp_prefilter() {
    let repo = Arc::new(MemoryDeviceRepo::new());
    let state = AppState::new(repo);
    let addr = spawn_server(state).await;
    let server = format!("http://{addr}");

    let (_sk, vk) = generate_device_keypair();
    let pk = vk.to_bytes();
    let reg = register_device(&server, &pk, Some("creds-otp-host"))
        .await
        .expect("register");

    let mut path = std::env::temp_dir();
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.push(format!("rl-creds-otp-{n}.json"));

    let file = HostCredentialFile::from_registration(&server, &reg);
    file.save(&path).expect("save");
    let loaded = HostCredentialFile::load(&path).expect("load");
    assert_eq!(loaded.public_id, reg.public_id);
    assert_eq!(loaded.access_token, reg.access_token);

    // Mint OTP and post hash — viewer prefilter must accept the code.
    let (code, hash) = mint_otp(6, DEFAULT_HOST_OTP_PEPPER).expect("mint");
    let mint = post_otp_hash(
        &server,
        &reg.public_id,
        &reg.access_token,
        &hex::encode(hash.digest),
        &hex::encode(hash.salt),
        hash.keyed,
        Some(300),
    )
    .await
    .expect("post otp");
    assert!(!mint.expires_at.is_empty());

    // Host hello
    let ws_url = http_to_ws_url(&server).unwrap();
    let mut host = SignalingClient::connect(&ws_url).await.unwrap();
    host.hello_host(&reg.access_token).await.unwrap();

    // Viewer hello + intent with real OTP
    let (mut viewer_ws, _) = connect_async(&ws_url).await.unwrap();
    let hello = SignalMessage::Hello {
        role: Role::Viewer,
        protocol_version: PROTOCOL_VERSION,
        auth: HelloAuth {
            device_token: String::new(),
        },
    };
    viewer_ws
        .send(Message::Text(encode_message(&hello).unwrap().into()))
        .await
        .unwrap();
    // hello_ok
    loop {
        let f = viewer_ws.next().await.unwrap().unwrap();
        if let Message::Text(t) = f {
            let _ = decode_message(t.as_str()).unwrap();
            break;
        }
    }

    let intent = SignalMessage::SessionIntent {
        session_id: "otp-sess-1".into(),
        signal_seq: 1,
        host_public_id: reg.public_id.clone(),
        mode: SessionMode::Otp,
        prefilter: serde_json::json!({ "otp": code.as_str() }),
    };
    viewer_ws
        .send(Message::Text(encode_message(&intent).unwrap().into()))
        .await
        .unwrap();

    // Host should receive session_incoming (prefilter passed).
    let incoming = host
        .recv_until(Duration::from_secs(5), |m| {
            matches!(m, SignalMessage::SessionIncoming { .. })
        })
        .await
        .expect("session_incoming after good OTP");
    match incoming {
        SignalMessage::SessionIncoming { session_id, .. } => {
            assert_eq!(session_id, "otp-sess-1");
        }
        other => panic!("unexpected {other:?}"),
    }

    let _ = std::fs::remove_file(&path);
}
