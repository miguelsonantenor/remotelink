//! Host WSS session agent: register → hello → accept → SDP/ICE → SessionManager media.
//!
//! Multi-process lab path (requires a running `remotelink-server`):
//!
//! ```text
//! remotelink-host --role=ws --server=http://127.0.0.1:8080 --transport=live
//! ```
//!
//! Prints `public_id` for the viewer. Uses **live** (or webrtc) PeerTransport —
//! mock is single-process only and is auto-upgraded to live with a warning.

use std::time::Duration;

use remotelink_auth::generate_device_keypair;
use remotelink_net::{
    create_peer_transport_with_config, PeerRole, TransportConfig, TransportMode,
};
use remotelink_protocol::SignalMessage;
use remotelink_signaling::{http_to_ws_url, register_device, SignalingClient};

use crate::session::{parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager};

/// Configuration for [`run_ws_host`].
#[derive(Debug, Clone)]
pub struct WsHostConfig {
    /// HTTP(S) base of the signaling server (`http://127.0.0.1:8080`).
    pub server: String,
    /// Optional display name for enrollment.
    pub display_name: String,
    /// Transport mode (mock is coerced to live).
    pub transport: TransportMode,
    /// Synthetic video frames to pump after connect.
    pub video_frames: u32,
    /// How long to wait for `session_incoming`.
    pub wait_incoming: Duration,
    /// When set, skip HTTP register and use these credentials (tests / restarts).
    pub existing: Option<ExistingHostCreds>,
}

/// Pre-enrolled host credentials (from a prior [`register_device`] call).
#[derive(Debug, Clone)]
pub struct ExistingHostCreds {
    /// Host public id.
    pub public_id: String,
    /// Access token for WSS hello.
    pub access_token: String,
}

impl Default for WsHostConfig {
    fn default() -> Self {
        Self {
            server: "http://127.0.0.1:8080".into(),
            display_name: "remotelink-host".into(),
            transport: TransportMode::Live,
            video_frames: 5,
            wait_incoming: Duration::from_secs(120),
            existing: None,
        }
    }
}

/// Register, connect WSS, accept one session, pump synthetic media.
///
/// Returns a human summary line on success.
pub async fn run_ws_host(cfg: WsHostConfig) -> Result<String, String> {
    let mut mode = cfg.transport;
    if mode == TransportMode::Mock || mode == TransportMode::Auto {
        eprintln!(
            "ws-host: transport `{}` is not multi-process safe; using live TCP",
            mode.as_str()
        );
        mode = TransportMode::Live;
    }
    let transport_cfg = TransportConfig { mode };

    let (public_id, access_token) = if let Some(ex) = &cfg.existing {
        println!(
            "ws-host: using existing public_id={} (viewer: --host {})",
            ex.public_id, ex.public_id
        );
        (ex.public_id.clone(), ex.access_token.clone())
    } else {
        let (_sk, vk) = generate_device_keypair();
        let pk = vk.to_bytes();
        let reg = register_device(&cfg.server, &pk, Some(&cfg.display_name))
            .await
            .map_err(|e| format!("register: {e}"))?;
        println!(
            "ws-host: registered public_id={} (viewer: --host {} --ws-connect)",
            reg.public_id, reg.public_id
        );
        (reg.public_id, reg.access_token)
    };

    let ws_url = http_to_ws_url(&cfg.server).map_err(|e| format!("ws url: {e}"))?;
    let mut sig = SignalingClient::connect(&ws_url)
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    let hello = sig
        .hello_host(&access_token)
        .await
        .map_err(|e| format!("hello: {e}"))?;
    if let SignalMessage::HelloOk { feature_flags, .. } = &hello {
        println!(
            "ws-host: hello_ok sdp_relay={}",
            feature_flags
                .get("sdp_relay")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        );
    }

    println!(
        "ws-host: waiting for session_incoming (timeout {}s)…",
        cfg.wait_incoming.as_secs()
    );
    let incoming = sig
        .recv_until(cfg.wait_incoming, |m| {
            matches!(m, SignalMessage::SessionIncoming { .. })
        })
        .await
        .map_err(|e| format!("wait incoming: {e}"))?;

    let session_id = match &incoming {
        SignalMessage::SessionIncoming { session_id, .. } => session_id.clone(),
        _ => unreachable!(),
    };
    println!("ws-host: session_incoming session_id={session_id}");

    // After intent(1)+incoming(2), client next_seq is 3 via observe_seq.
    let accept_seq = sig.take_seq().max(3);
    if sig.next_seq <= accept_seq {
        sig.next_seq = accept_seq.saturating_add(1);
    }
    sig.send(&SignalMessage::SessionAccept {
        session_id: session_id.clone(),
        signal_seq: accept_seq,
    })
    .await
    .map_err(|e| format!("accept: {e}"))?;
    println!("ws-host: session_accept seq={accept_seq}");

    // Media plane: offerer PeerTransport + SessionManager.
    let offerer = create_peer_transport_with_config(PeerRole::Offerer, &transport_cfg)
        .map_err(|e| format!("create offerer: {e}"))?;
    let mut mgr = SessionManager::with_peer(offerer);
    mgr.attach(&session_id);
    mgr.start_media().map_err(|e| format!("start_media: {e}"))?;

    let outbound = mgr.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .ok_or_else(|| "no session_offer from SessionManager".to_string())?;
    let offer = parse_sdp_payload(&offer_sig.payload).map_err(|e| e.to_string())?;

    let offer_seq = sig.take_seq();
    sig.send(&SignalMessage::SessionOffer {
        session_id: session_id.clone(),
        signal_seq: offer_seq,
        sdp: offer.sdp.clone(),
        fingerprint_sig: offer.fingerprint_sig.clone().unwrap_or_default(),
    })
    .await
    .map_err(|e| format!("send offer: {e}"))?;
    println!("ws-host: session_offer seq={offer_seq} sdp_len={}", offer.sdp.len());

    // Collect host ICE but **do not trickle until after session_answer**.
    // Strict signal_seq is session-global; racing ICE with the viewer's answer
    // causes stale_signal_seq (host ICE claims seq N while viewer still answers N).
    let mut pending_host_ice: Vec<_> = outbound
        .iter()
        .filter(|s| s.kind == signal_kind::ICE_CANDIDATE)
        .cloned()
        .collect();

    // Wait for answer first (viewer may also send ICE after its answer).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut got_answer = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match sig.recv_timeout(remaining.min(Duration::from_millis(250))).await {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => continue,
            Err(e) => return Err(format!("recv: {e}")),
        };

        match msg {
            SignalMessage::SessionAnswer { sdp, signal_seq, .. } => {
                println!("ws-host: session_answer seq={signal_seq}");
                mgr.apply_signal(
                    signal_kind::SESSION_ANSWER,
                    &serde_json::to_string(&SdpPayload {
                        sdp,
                        fingerprint_sig: None,
                    })
                    .map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply answer: {e}"))?;
                got_answer = true;
                break;
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                // Viewer ICE can arrive after its answer in the same poll window.
                mgr.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply ice: {e}"))?;
            }
            other => {
                println!("ws-host: ignore {other:?}");
            }
        }
    }

    if !got_answer {
        return Err("timeout waiting for session_answer".into());
    }

    // Now trickle host ICE (offer-time + any newly queued).
    let _ = mgr.peer_mut().poll();
    pending_host_ice.extend(
        mgr.take_outbound_signals()
            .into_iter()
            .filter(|s| s.kind == signal_kind::ICE_CANDIDATE),
    );
    for ice_sig in pending_host_ice {
        let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
        let ice_seq = sig.take_seq();
        sig.send(&SignalMessage::IceCandidate {
            session_id: session_id.clone(),
            signal_seq: ice_seq,
            candidate: c,
        })
        .await
        .map_err(|e| format!("send host ice: {e}"))?;
    }

    // Drain remaining ICE + wait Connected.
    let ice_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < ice_deadline {
        let _ = mgr.peer_mut().poll();
        for ice_sig in mgr.take_outbound_signals() {
            if ice_sig.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
                let ice_seq = sig.take_seq();
                sig.send(&SignalMessage::IceCandidate {
                    session_id: session_id.clone(),
                    signal_seq: ice_seq,
                    candidate: c,
                })
                .await
                .map_err(|e| format!("send host ice: {e}"))?;
            }
        }
        if let Ok(msg) = sig.recv_timeout(Duration::from_millis(50)).await {
            if let SignalMessage::IceCandidate { candidate, .. } = msg {
                mgr.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply ice: {e}"))?;
            }
        }
        if mgr.connection_state() == remotelink_net::ConnectionState::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    mgr.wait_ready(Duration::from_secs(10))
        .map_err(|e| format!("wait_ready: {e}"))?;
    let pump = mgr
        .pump_media(cfg.video_frames)
        .map_err(|e| format!("pump: {e}"))?;
    if pump.skipped_not_connected || pump.video_sent == 0 {
        return Err(format!(
            "pump failed (skipped={} video_sent={})",
            pump.skipped_not_connected, pump.video_sent
        ));
    }

    let fp = mgr
        .peer_mut()
        .local_fingerprint()
        .map_err(|e| e.to_string())?;
    let summary = format!(
        "ws-host ok public_id={} session={} transport={} video_tx={} audio_tx={} fp={}",
        public_id,
        session_id,
        mode.as_str(),
        pump.video_sent,
        pump.audio_sent,
        fp.as_sign_material()
    );

    let _ = mgr.shutdown();
    let _ = sig.close().await;
    Ok(summary)
}

/// Blocking entry for the host binary.
pub fn run_ws_host_blocking(cfg: WsHostConfig) -> Result<String, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    rt.block_on(run_ws_host(cfg))
}
