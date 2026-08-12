//! Viewer WSS connect: hello → session_intent → answer → receive media.
//!
//! ```text
//! remotelink-viewer --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp 123456 --transport=live
//! ```

use std::time::Duration;

use remotelink_net::{
    create_peer_transport_with_config, ConnectionState, PeerRole, SessionDescription,
    TransportConfig, TransportMode,
};
use remotelink_protocol::{SessionMode, SignalMessage};
use remotelink_signaling::{http_to_ws_url, SignalingClient};
use remotelink_viewer_core::{ConnectRequest, ViewerSession};

/// Viewer WSS connect configuration.
#[derive(Debug, Clone)]
pub struct WsViewerConfig {
    /// HTTP(S) base of the signaling server.
    pub server: String,
    /// Host public id to call.
    pub host_public_id: String,
    /// OTP code for Mode A intent (prefilter; optional if host has no published OTP).
    pub otp: String,
    /// Transport mode (mock coerced to live).
    pub transport: TransportMode,
    /// How long to wait for media after ICE.
    pub media_timeout: Duration,
}

impl Default for WsViewerConfig {
    fn default() -> Self {
        Self {
            server: "http://127.0.0.1:18080".into(),
            host_public_id: String::new(),
            otp: "123456".into(),
            transport: TransportMode::Live,
            media_timeout: Duration::from_secs(30),
        }
    }
}

/// Connect as anonymous viewer, complete signaling, receive synthetic media.
pub async fn run_ws_viewer(cfg: WsViewerConfig) -> Result<String, String> {
    if cfg.host_public_id.is_empty() {
        return Err("--host PUBLIC_ID is required for --ws-connect".into());
    }
    let mut mode = cfg.transport;
    if mode == TransportMode::Mock || mode == TransportMode::Auto {
        eprintln!(
            "ws-viewer: transport `{}` is not multi-process safe; using live TCP",
            mode.as_str()
        );
        mode = TransportMode::Live;
    }
    let transport_cfg = TransportConfig { mode };

    let ws_url = http_to_ws_url(&cfg.server).map_err(|e| format!("ws url: {e}"))?;
    let mut sig = SignalingClient::connect(&ws_url)
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    sig.hello_viewer_anonymous()
        .await
        .map_err(|e| format!("hello: {e}"))?;
    println!("ws-viewer: hello_ok");

    let session_id = format!("viewer-{}", &uuid_like());
    let intent_seq = sig.take_seq();
    let req = ConnectRequest::otp(&cfg.host_public_id, &cfg.otp);
    let intent = req
        .session_intent_message(&session_id, intent_seq)
        .map_err(|e| format!("intent: {e}"))?;
    // Ensure mode is otp for prefilter
    let intent = match intent {
        SignalMessage::SessionIntent {
            session_id,
            signal_seq,
            host_public_id,
            mode: _,
            prefilter,
        } => SignalMessage::SessionIntent {
            session_id,
            signal_seq,
            host_public_id,
            mode: SessionMode::Otp,
            prefilter,
        },
        other => other,
    };
    sig.send(&intent)
        .await
        .map_err(|e| format!("send intent: {e}"))?;
    println!("ws-viewer: session_intent session_id={session_id} seq={intent_seq}");

    let accept = sig
        .recv_until(Duration::from_secs(60), |m| {
            matches!(
                m,
                SignalMessage::SessionAccept { .. } | SignalMessage::SessionReject { .. }
            )
        })
        .await
        .map_err(|e| format!("wait accept: {e}"))?;
    match accept {
        SignalMessage::SessionAccept { signal_seq, .. } => {
            println!("ws-viewer: session_accept seq={signal_seq}");
        }
        SignalMessage::SessionReject { reason, .. } => {
            return Err(format!("session rejected: {reason:?}"));
        }
        _ => unreachable!(),
    }

    let answerer = create_peer_transport_with_config(PeerRole::Answerer, &transport_cfg)
        .map_err(|e| format!("create answerer: {e}"))?;
    let mut viewer = ViewerSession::new();
    viewer
        .begin_connect(&req)
        .map_err(|e| format!("begin_connect: {e}"))?;
    viewer.attach_transport(answerer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut got_offer = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match sig.recv_timeout(remaining.min(Duration::from_millis(250))).await {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {
                let _ = viewer.poll();
                continue;
            }
            Err(e) => return Err(format!("recv: {e}")),
        };

        match msg {
            SignalMessage::SessionOffer {
                sdp,
                fingerprint_sig,
                signal_seq,
                ..
            } => {
                println!(
                    "ws-viewer: session_offer seq={signal_seq} sdp_len={}",
                    sdp.len()
                );
                let answer = viewer
                    .accept_offer_with_sig(
                        SessionDescription::offer(sdp),
                        if fingerprint_sig.is_empty() {
                            None
                        } else {
                            Some(fingerprint_sig.as_str())
                        },
                    )
                    .map_err(|e| format!("accept_offer: {e}"))?;
                let ans_seq = sig.take_seq();
                sig.send(&SignalMessage::SessionAnswer {
                    session_id: session_id.clone(),
                    signal_seq: ans_seq,
                    sdp: answer.sdp,
                })
                .await
                .map_err(|e| format!("send answer: {e}"))?;
                println!("ws-viewer: session_answer seq={ans_seq}");
                got_offer = true;

                // Send local ICE
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .map_err(|e| format!("send ice: {e}"))?;
                }
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                viewer
                    .add_remote_ice(candidate)
                    .map_err(|e| format!("add ice: {e}"))?;
                let _ = viewer.poll();
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .map_err(|e| format!("send ice: {e}"))?;
                }
            }
            other => println!("ws-viewer: ignore {other:?}"),
        }

        let _ = viewer.poll();
        if got_offer && viewer.transport_state() == Some(ConnectionState::Connected) {
            break;
        }
    }

    if !got_offer {
        return Err("timeout waiting for session_offer".into());
    }

    // Wait for media
    let media_deadline = tokio::time::Instant::now() + cfg.media_timeout;
    while tokio::time::Instant::now() < media_deadline {
        let _ = viewer.poll();
        if !viewer.recorded_video_nalus().is_empty()
            && !viewer.recorded_audio_packets().is_empty()
        {
            break;
        }
        // Drain any late ICE while waiting
        if let Ok(msg) = sig.recv_timeout(Duration::from_millis(50)).await {
            if let SignalMessage::IceCandidate { candidate, .. } = msg {
                let _ = viewer.add_remote_ice(candidate);
            }
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    let v = viewer.recorded_video_nalus().len();
    let a = viewer.recorded_audio_packets().len();
    if v == 0 {
        return Err(format!(
            "no video received (audio={a} state={:?})",
            viewer.transport_state()
        ));
    }

    let summary = format!(
        "ws-viewer ok host={} session={} transport={} video_rx={v} audio_rx={a}",
        cfg.host_public_id,
        session_id,
        mode.as_str()
    );
    let _ = sig.close().await;
    Ok(summary)
}

/// Blocking entry for the viewer binary.
pub fn run_ws_viewer_blocking(cfg: WsViewerConfig) -> Result<String, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    rt.block_on(run_ws_viewer(cfg))
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{t:x}")
}
